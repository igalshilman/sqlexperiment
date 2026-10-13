// The addon over an in-memory ordered store: SQL, values, and lost writes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { open } = require('../index.js');

/** Stored values, and writes acknowledged but not yet confirmed. */
class Backend {
  stored = new Map();
  unconfirmed = [];

  confirm() {
    for (const [key, value] of this.unconfirmed.splice(0)) {
      if (value === undefined) this.stored.delete(key);
      else this.stored.set(key, value);
    }
  }

  lose() {
    this.unconfirmed.length = 0;
  }

  /** A request's view: its own writes over the stored values. */
  request({ fails = false } = {}) {
    const backend = this;
    return {
      async get(key) {
        for (let i = backend.unconfirmed.length - 1; i >= 0; i--) {
          if (backend.unconfirmed[i][0] === key) return backend.unconfirmed[i][1];
        }
        return backend.stored.get(key);
      },
      set(key, value) {
        backend.unconfirmed.push([key, Uint8Array.from(value)]);
      },
      clear(key) {
        backend.unconfirmed.push([key, undefined]);
      },
      async sync() {
        if (fails) {
          backend.lose();
          throw new Error('the request failed');
        }
        backend.confirm();
      },
    };
  }
}

let namespaces = 0;
const fresh = () => `namespace-${namespaces++}`;

test('runs SQL and round-trips values', async () => {
  const backend = new Backend();
  const ns = fresh();
  const db = await open(ns, backend.request());
  await db.execute('CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n INTEGER)');
  await db.execute('INSERT INTO t VALUES(?, ?, ?, ?, ?)', [
    42,
    1.5,
    'héllo',
    Uint8Array.from([1, 2, 3]),
    null,
  ]);
  await db.execute('INSERT INTO t VALUES(?, ?, ?, ?, ?)', [2n ** 62n, -0.25, '', new Uint8Array(0), true]);
  const rows = await db.query('SELECT * FROM t ORDER BY i');
  assert.deepEqual(rows[0], [42, 1.5, 'héllo', Buffer.from([1, 2, 3]), null]);
  assert.deepEqual(rows[1], [2n ** 62n, -0.25, '', Buffer.alloc(0), 1]);
  await db.save();

  // Another session, and another process's view (no cached state), read it.
  for (const name of [ns, fresh()]) {
    const again = await open(name, backend.request());
    assert.deepEqual(await again.query('SELECT count(*) FROM t'), [[2]]);
    await again.save();
  }
});

test('an SQL error leaves the session usable; a transaction rolls back', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY)');
  await assert.rejects(db.query('SELECT * FROM missing'), /no such table/);
  assert.equal(db.isUsable(), true);
  await assert.rejects(
    db.transaction(async (tx) => {
      await tx.execute('INSERT INTO t VALUES(1)');
      throw new Error('abort');
    }),
    /abort/,
  );
  assert.deepEqual(await db.query('SELECT count(*) FROM t'), [[0]]);
  assert.equal(await db.transaction(async (tx) => tx.query('INSERT INTO t VALUES(2) RETURNING id')).then((r) => r[0][0]), 2);
  await db.save();
});

/**
 * Requests insert a row each and save; request `lost` fails and loses its
 * writes, its confirmation failing or the session closed first. The owner and
 * a process without cached state must then read exactly the stored rows.
 */
async function consistentAfterLoss(lost, closeFirst) {
  const backend = new Backend();
  const ns = fresh();
  let db = await open(ns, backend.request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY, v BLOB)');
  await db.save();
  let expected = 0;
  for (let id = 0; id < 20; id++) {
    const fails = id === lost;
    db = await open(ns, backend.request({ fails }));
    await db.execute('INSERT INTO t VALUES(?, randomblob(3000))', [id]);
    if (!fails) {
      await db.save();
      expected++;
    } else if (closeFirst) {
      await db.close();
      backend.lose();
    } else {
      await assert.rejects(db.save(), /the request failed/);
    }
  }
  for (const name of [ns, fresh()]) {
    const view = await open(name, backend.request());
    const [[rows]] = await view.query('SELECT count(*) FROM t');
    const [[integrity]] = await view.query('PRAGMA integrity_check');
    await view.save();
    if (rows !== expected || integrity !== 'ok') return false;
  }
  return true;
}

test('lost writes leave every reader consistent', async () => {
  for (const closeFirst of [false, true]) {
    for (let lost = 1; lost < 15; lost++) {
      assert.ok(await consistentAfterLoss(lost, closeFirst), `lost request ${lost}, closed first: ${closeFirst}`);
    }
  }
});

test('a write the store rejects is reported by the next sync', async () => {
  const backend = new Backend();
  const store = backend.request();
  store.set = () => {
    throw new Error('state is closed');
  };
  const db = await open(fresh(), store);
  await db.execute('CREATE TABLE t(id INTEGER)');
  await assert.rejects(db.save(), /state is closed/);
});

test('a transaction holds the session: other calls wait until it ends', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY)');
  let handle;
  const rolledBack = db.transaction(async (tx) => {
    handle = tx;
    await tx.execute('INSERT INTO t VALUES(1)');
    await new Promise((resolve) => setTimeout(resolve, 10));
    throw new Error('abort');
  });
  // Issued while the transaction waits: neither runs inside it.
  const write = db.execute('INSERT INTO t VALUES(2)');
  const committed = db.transaction(async (tx) => tx.execute('INSERT INTO t VALUES(3)'));
  await assert.rejects(rolledBack, /abort/);
  await write;
  await committed;
  assert.deepEqual(await db.query('SELECT id FROM t ORDER BY id'), [[2], [3]]);
  await assert.rejects(async () => handle.query('SELECT 1'), /the transaction has ended/);
  await db.save();
});

test('a COMMIT that fails rolls the transaction back', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('PRAGMA foreign_keys=ON');
  await db.execute('CREATE TABLE p(id INTEGER PRIMARY KEY)');
  await db.execute('CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)');
  // The deferred constraint fails at COMMIT, not at the INSERT.
  await assert.rejects(
    db.transaction(async (tx) => tx.execute('INSERT INTO c VALUES(7)')),
    /FOREIGN KEY constraint failed/,
  );
  assert.ok(db.isUsable());
  // Nothing of it is left, and the next transaction can begin.
  assert.deepEqual(await db.query('SELECT * FROM c'), []);
  await db.transaction(async (tx) => tx.execute('INSERT INTO p VALUES(7)'));
  assert.deepEqual(await db.query('SELECT id FROM p'), [[7]]);
  await db.save();
});

test('a call to the session inside its transaction is refused, not left waiting', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  let later;
  await db.transaction(async (tx) => {
    await tx.execute('INSERT INTO t VALUES(1)');
    // It would wait for this transaction, which waits for this body.
    await assert.rejects(db.execute('INSERT INTO t VALUES(2)'), /use the transaction handle/);
    await assert.rejects(db.transaction(async () => {}), /use the transaction handle/);
    // Once the body has settled, its asynchronous context may use the session.
    later = new Promise((resolve) => setTimeout(resolve, 5)).then(() => db.execute('INSERT INTO t VALUES(3)'));
  });
  await later;
  assert.deepEqual(await db.query('SELECT id FROM t ORDER BY id'), [[1], [3]]);
  await db.save();
});

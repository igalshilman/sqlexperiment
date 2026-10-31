// A database over an in-memory store, without the SDK: SQL, values, records,
// and lost writes.
import assert from 'node:assert/strict';
import { Buffer } from 'node:buffer';
import { open, type Database, type Store, type Transaction } from 'restate-objects/internal';
import { test } from 'vitest';

/** Stored values, and writes issued but not yet synced. */
class Backend {
  stored = new Map<string, Uint8Array>();
  unconfirmed: [key: string, value: Uint8Array | undefined][] = [];
  // How many unsynced writes a failure keeps, in order: none, or a pseudo-random prefix.
  #seed = 0;

  constructor({ seed = 0 } = {}) {
    this.#seed = seed;
  }

  confirm() {
    for (const [key, value] of this.unconfirmed.splice(0)) {
      if (value === undefined) this.stored.delete(key);
      else this.stored.set(key, value);
    }
  }

  /** A failure: the store keeps a prefix of the unsynced writes, the most the contract allows. */
  lose() {
    let kept = 0;
    if (this.#seed !== 0) {
      this.#seed = (this.#seed * 1103515245 + 12345) % 2 ** 31;
      kept = this.#seed % (this.unconfirmed.length + 1);
    }
    this.unconfirmed.splice(kept);
    this.confirm();
  }

  /** What a request reads: its own latest write, or the stored value. */
  read(key: string): Uint8Array | undefined {
    for (let i = this.unconfirmed.length - 1; i >= 0; i--) {
      const [written, value] = this.unconfirmed[i]!;
      if (written === key) return value;
    }
    return this.stored.get(key);
  }

  /** A request's view: its own writes over the stored values. */
  request({ fails = false } = {}): Store {
    return {
      get: async (key) => this.read(key),
      set: (key, value) => void this.unconfirmed.push([key, Uint8Array.from(value)]),
      clear: (key) => void this.unconfirmed.push([key, undefined]),
      sync: async () => {
        if (fails) {
          this.lose();
          throw new Error('the request failed');
        }
        this.confirm();
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
  await db.execute('INSERT INTO t VALUES(?, ?, ?, ?, ?)', [42, 1.5, 'héllo', Uint8Array.from([1, 2, 3]), null]);
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
  assert.equal(
    await db.transaction(async (tx) => tx.query('INSERT INTO t VALUES(2) RETURNING id')).then((r) => r[0]![0]),
    2,
  );
  await db.save();
});

/**
 * Requests insert a row each; request `lost` fails, its sync losing some of
 * its writes (none kept, or a prefix by `seed`). The owner and a process
 * without cached state must then read the rows of the other requests, and
 * the lost one's only if both do.
 */
async function consistentAfterLoss(lost: number, seed: number) {
  const backend = new Backend({ seed });
  const ns = fresh();
  let db = await open(ns, backend.request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY, v BLOB)');
  await db.save();
  for (let id = 0; id < 20; id++) {
    const fails = id === lost;
    db = await open(ns, backend.request({ fails }));
    const insert = db.execute('INSERT INTO t VALUES(?, randomblob(3000))', [id]);
    if (fails) {
      await assert.rejects(insert, /the request failed/);
      await db.close();
    } else {
      await insert;
      await db.save();
    }
  }
  const seen: number[] = [];
  for (const name of [ns, fresh()]) {
    const view = await open(name, backend.request());
    const [[rows, landed]] = (await view.query('SELECT count(*), count(*) FILTER (WHERE id = ?) FROM t', [lost])) as [
      [number, number],
    ];
    const [[integrity]] = (await view.query('PRAGMA integrity_check')) as [[string]];
    await view.save();
    if (rows !== 19 + landed || integrity !== 'ok') return false;
    seen.push(landed);
  }
  return seen[0] === seen[1];
}

test('lost writes leave every reader consistent', { timeout: 120_000 }, async () => {
  for (const seed of [0, 1, 2, 3, 4, 5]) {
    for (let lost = 1; lost < 15; lost++) {
      assert.ok(await consistentAfterLoss(lost, seed), `lost request ${lost}, seed ${seed}`);
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
  await assert.rejects(db.execute('CREATE TABLE t(id INTEGER)'), /state is closed/);
  assert.equal(db.isUsable(), false);
  await db.close();
});

test('a store of a class has its methods called on it', async () => {
  class MapStore implements Store {
    private readonly values = new Map<string, Uint8Array>();
    async get(key: string) {
      return this.values.get(key);
    }
    set(key: string, value: Uint8Array) {
      this.values.set(key, Uint8Array.from(value));
    }
    clear(key: string) {
      this.values.delete(key);
    }
    async sync() {}
    async keys() {
      return [...this.values.keys()];
    }
  }
  const db = await open(fresh(), new MapStore());
  await db.execute('CREATE TABLE t(id INTEGER)');
  await db.execute('INSERT INTO t VALUES (1)');
  assert.deepEqual(await db.query('SELECT id FROM t'), [[1]]);
  await db.save();
});

test('a transaction holds the session: other calls wait until it ends', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY)');
  let handle: Transaction | undefined;
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
  await assert.rejects(async () => handle!.query('SELECT 1'), /the transaction has ended/);
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

test('a statement that ends the transaction of a body ends the session, and commits nothing', async () => {
  const backend = new Backend();
  const ns = fresh();
  const db = await open(ns, backend.request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  for (const end of ['COMMIT', 'ROLLBACK', 'END']) {
    const session = await open(ns, backend.request());
    await assert.rejects(
      session.transaction(async (tx) => {
        await tx.execute('INSERT INTO t VALUES(1)');
        await tx.execute(end);
      }),
      /ended the transaction/,
    );
    assert.equal(session.isUsable(), false);
  }
  assert.deepEqual(await db.query('SELECT count(*) FROM t'), [[0]]);
  await db.save();
});

test('save refuses a transaction left open, whose changes are lost', async () => {
  const backend = new Backend();
  const ns = fresh();
  const db = await open(ns, backend.request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  await db.execute('BEGIN');
  await db.execute('INSERT INTO t VALUES(1)');
  await assert.rejects(db.save(), /left open/);
  const again = await open(ns, backend.request());
  assert.deepEqual(await again.query('SELECT count(*) FROM t'), [[0]]);
  await again.save();
});

test('records a commit cannot hold are refused, and the session goes on', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  await assert.rejects(
    db.transaction(async (tx) => {
      await tx.execute('INSERT INTO t VALUES(1)');
      tx.setRecord('outbox', new Uint8Array((16 << 20) + 1));
    }),
    /exceed/,
  );
  await assert.rejects(
    db.transaction(async (tx) => {
      await tx.execute('INSERT INTO t VALUES(1)');
      tx.setRecord('db', new Uint8Array());
    }),
    /cannot name a record/,
  );
  assert.ok(db.isUsable());
  assert.deepEqual(await db.query('SELECT count(*) FROM t'), [[0]]);
  await db.save();
});

test('a call running meanwhile does not make the session look unusable', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  for (let i = 0; i < 50; i++) {
    // The body fails while record() holds the session: the rollback still runs.
    await assert.rejects(
      db.transaction(async (tx) => {
        await tx.execute('INSERT INTO t VALUES(1)');
        for (let j = 0; j < 4; j++) void db.record('outbox');
        throw new Error('no');
      }),
      /no/,
    );
  }
  assert.deepEqual(await db.query('SELECT count(*) FROM t'), [[0]]);
  // Nor while a statement runs.
  const running = db.query(
    'WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000) SELECT count(*) FROM n',
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(db.isUsable());
  await running;
  await db.save();
  assert.equal(db.isUsable(), false);
});

/** A session's record `name`, as text. */
const text = async (session: Database, name = 'outbox') => new TextDecoder().decode(await session.record(name));

const bytes = (value: string) => new TextEncoder().encode(value);

test('records commit with their transaction, and only then', async () => {
  const backend = new Backend();
  const namespace = fresh();
  const db = await open(namespace, backend.request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  assert.equal(await text(db), '');
  // Even with a transaction that changes nothing; two records at once.
  let inside: string | undefined;
  await db.transaction(async (tx) => {
    await tx.query('SELECT * FROM t');
    tx.setRecord('outbox', bytes('sends 1'));
    tx.setRecord('schema', bytes('1'));
    inside = await text(db);
  });
  assert.equal(inside, '');
  assert.equal(await text(db), 'sends 1');
  assert.equal(await text(db, 'schema'), '1');
  // A rollback commits none; nor does a commit without records change them.
  await assert.rejects(
    db.transaction(async (tx) => {
      await tx.execute('INSERT INTO t VALUES(2)');
      tx.setRecord('outbox', bytes('sends 2'));
      throw new Error('no');
    }),
    /no/,
  );
  await db.transaction((tx) => tx.execute('INSERT INTO t VALUES(3)'));
  assert.equal(await text(db), 'sends 1');
  await db.save();
  // A later session opens with them; setting one empty clears it alone.
  const again = await open(namespace, backend.request());
  assert.equal(await text(again), 'sends 1');
  await again.transaction(async (tx) => tx.setRecord('outbox', new Uint8Array()));
  assert.equal(await text(again), '');
  assert.equal(await text(again, 'schema'), '1');
  await again.save();
});

test('a call to the session inside its transaction is refused, not left waiting', async () => {
  const db = await open(fresh(), new Backend().request());
  await db.execute('CREATE TABLE t(id INTEGER)');
  let later: Promise<void> = Promise.resolve();
  await db.transaction(async (tx) => {
    await tx.execute('INSERT INTO t VALUES(1)');
    // It would wait for this transaction, which waits for this body.
    await assert.rejects(db.execute('INSERT INTO t VALUES(2)'), /use the transaction handle/);
    await assert.rejects(
      db.transaction(async () => {}),
      /use the transaction handle/,
    );
    // Once the body has settled, its asynchronous context may use the session.
    later = new Promise((resolve) => setTimeout(resolve, 5)).then(() => db.execute('INSERT INTO t VALUES(3)'));
  });
  await later;
  assert.deepEqual(await db.query('SELECT id FROM t ORDER BY id'), [[1], [3]]);
  await db.save();
});

test('a call to a session inside a transaction of another, inside its own, is refused', async () => {
  const a = await open(fresh(), new Backend().request());
  const b = await open(fresh(), new Backend().request());
  await a.transaction(async () => {
    await b.transaction(async (tx) => {
      await tx.query('SELECT 1');
      // `a` waits for its transaction, which waits for this body.
      await assert.rejects(a.query('SELECT 1'), /use the transaction handle/);
    });
    // Once b's transaction has ended, b may be used again from a's body.
    assert.deepEqual(await b.query('SELECT 1'), [[1]]);
  });
  await a.save();
  await b.save();
});

test('a statement that commits resolves once its writes are stored, however many', async () => {
  const backend = new Backend();
  const db = await open(fresh(), backend.request());
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY, b BLOB)');
  // Thousands of page writes: more than Node.js dispatches from a queue in one turn.
  await db.execute(
    'INSERT INTO t(b) SELECT randomblob(3000) FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000) SELECT x FROM c)',
  );
  assert.equal(backend.unconfirmed.length, 0);
  const tail = [...backend.stored.keys()].filter((key) => key.startsWith('v22/tail/')).length;
  assert.ok(tail > 8, `${tail} tail parts stored`);
  await db.save();
});

/** How many pack parts `backend` stores. */
const packs = (backend: Backend) => [...backend.stored.keys()].filter((key) => key.startsWith('v22/pack/')).length;

/** The pack parts left after 100 commits that each replace a row, with a store that lists its keys or not. */
const packsAfterCommits = async (listing: boolean) => {
  const backend = new Backend();
  const store = backend.request();
  // As the object's ctx.stateKeys().
  if (listing) store.keys = async () => [...backend.stored.keys(), ...backend.unconfirmed.map(([key]) => key)];
  const db = await open(fresh(), store);
  await db.execute('CREATE TABLE t(id INTEGER PRIMARY KEY, b BLOB)');
  for (let i = 0; i < 100; i++) await db.execute('INSERT OR REPLACE INTO t VALUES(1, randomblob(3000))');
  await db.save();
  return packs(backend);
};

test('a store that lists its keys is collected; one that cannot keeps its packs', async () => {
  const [collected, kept] = [await packsAfterCommits(true), await packsAfterCommits(false)];
  assert.ok(collected < 5 && kept > 10, `${collected} pack parts left collected, ${kept} kept`);
});

for (const { how, keys } of [
  { how: 'rejects', keys: async () => Promise.reject(new Error('listing failed')) },
  {
    how: 'throws',
    keys: (): Promise<string[]> => {
      throw new Error('listing failed');
    },
  },
]) {
  test(`a listing that ${how} fails the commit that collects, which landed`, async () => {
    const backend = new Backend();
    const ns = fresh();
    const store = backend.request();
    store.keys = keys;
    const db = await open(ns, store);
    await db.execute('CREATE TABLE t(b BLOB)');
    let committed = 1;
    // A commit that checkpoints collects: once the tail holds a few rows.
    await assert.rejects(async () => {
      for (; committed < 40; committed++) await db.execute('INSERT INTO t VALUES (randomblob(3000))');
    }, /listing failed/);
    const reopened = await open(ns, backend.request());
    assert.deepEqual(await reopened.query('SELECT count(*) FROM t'), [[committed]]);
    await reopened.close();
  });
}

/** A reader's view of `backend`: what is stored and written so far, and no writes. */
const readerOf = (backend: Backend): Store => ({
  get: async (key) => backend.read(key),
  set() {
    throw new Error('a reader wrote');
  },
  clear() {
    throw new Error('a reader wrote');
  },
  async sync() {},
});

test('a reader reads beside the owner, writes nothing, and keeps the state it opened on', async () => {
  const backend = new Backend();
  const ns = fresh();
  const owner = await open(ns, backend.request());
  await owner.execute('CREATE TABLE t(n INTEGER)');
  await owner.execute('INSERT INTO t VALUES (1)');

  const reader = await open(ns, readerOf(backend), { reader: true });
  assert.deepEqual(await reader.query('SELECT n FROM t'), [[1]]);
  // Its own changes stay in the session.
  await reader.execute('CREATE TABLE IF NOT EXISTS t(n INTEGER)');
  await reader.execute('INSERT INTO t VALUES (99)');
  assert.deepEqual(await reader.query('SELECT n FROM t ORDER BY n'), [[1], [99]]);
  // Its records, too.
  await reader.transaction(async (tx) => tx.setRecord('outbox', bytes('mine')));
  assert.equal(await text(reader), 'mine');

  // The owner commits: the reader keeps the state it opened on.
  await owner.execute('INSERT INTO t VALUES (2)');
  assert.deepEqual(await reader.query('SELECT n FROM t ORDER BY n'), [[1], [99]]);
  await reader.close();

  const again = await open(ns, readerOf(backend), { reader: true });
  assert.deepEqual(await again.query('SELECT n FROM t ORDER BY n'), [[1], [2]]);
  assert.equal(await text(again), '');
  await again.close();
  await owner.save();
});

test('a reader of a namespace without a database reads none', async () => {
  const reader = await open(fresh(), readerOf(new Backend()), { reader: true });
  assert.deepEqual(await reader.query('SELECT count(*) FROM sqlite_schema'), [[0]]);
  await reader.close();
});

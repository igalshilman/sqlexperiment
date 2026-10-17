'use strict';
// SQL over a key/value store implemented in JavaScript. See index.d.ts.
const { AsyncLocalStorage } = require('node:async_hooks');
const addon = require('./sqlite_kv.node');

const GET = 0;
const SET = 1;
const CLEAR = 2;
const SYNC = 3;
const BARRIER = 4;

/**
 * Open the database of `namespace` over `store`. Every store call goes through
 * one queue, so the store sees them in the order they were issued. A `reader`
 * reads beside the namespace's owner, and writes nothing.
 */
async function open(namespace, store, { reader = false } = {}) {
  // A failed write cannot fail its caller, which has moved on: the next sync
  // reports it, so the session's writes are not confirmed.
  let failure = null;
  // The native side passes [op, key, value].
  const dispatch = ([op, key, value]) => {
    switch (op) {
      case GET:
        return Promise.resolve(store.get(key)).then((v) => v ?? null);
      case SET:
      case CLEAR:
        try {
          if (op === SET) store.set(key, value);
          else store.clear(key);
        } catch (error) {
          failure ??= error;
        }
        return undefined;
      case SYNC:
        if (failure) return Promise.reject(failure);
        return Promise.resolve(store.sync()).then(() => null);
      case BARRIER:
        // Reached after every write queued before it.
        return Promise.resolve(null);
      default:
        return Promise.reject(new Error(`unknown store operation ${op}`));
    }
  };
  return new Database(await addon.open(namespace, dispatch, reader));
}

// The transaction whose body is running, in that body's asynchronous context:
// the innermost, which knows the ones it runs inside.
const running = new AsyncLocalStorage();

class Database {
  #native;
  // Calls run one at a time, in call order: a transaction holds the session
  // from BEGIN to COMMIT or ROLLBACK, so no other call runs inside it.
  #tail = Promise.resolve();

  constructor(nativeDatabase) {
    this.#native = nativeDatabase;
  }

  #serial(operation) {
    // A call to the session from inside one of its transactions' bodies would
    // wait for that transaction, which waits for the body: refuse it.
    if (running.getStore()?.holds(this)) {
      return Promise.reject(new Error('inside a transaction, use the transaction handle its body gets'));
    }
    const result = this.#tail.then(operation);
    this.#tail = result.catch(() => {});
    return result;
  }

  /** Run SQL and return its rows; parameters bind to a single statement. */
  query(sql, params) {
    return this.#serial(() => this.#native.query(sql, params));
  }

  /** Run SQL and return its rows with the column names of their statement. */
  queryColumns(sql, params) {
    return this.#serial(() => this.#native.queryColumns(sql, params));
  }

  /** Run SQL, discarding rows. */
  async execute(sql, params) {
    await this.query(sql, params);
  }

  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, and
   * rolls back when it rejects. Calls to the database wait until it ends, so
   * `body` uses the handle it gets, which is invalid once `body` settles.
   */
  transaction(body) {
    return this.#serial(async () => {
      const native = this.#native;
      await native.query('BEGIN');
      const tx = new Transaction(this, native, running.getStore());
      let result;
      try {
        result = await running.run(tx, () => body(tx));
      } catch (error) {
        tx.end();
        await rollback(native);
        throw error;
      }
      const record = tx.recorded;
      tx.end();
      try {
        await native.commit(record);
      } catch (error) {
        // A COMMIT that fails (a deferred constraint) leaves the transaction
        // open: without the rollback, the calls after it would run inside it.
        await rollback(native);
        throw error;
      }
      return result;
    });
  }

  /**
   * The database's version: it grows with each commit that changes the
   * database, or whose transaction called `recordCommit()`, and only then. It
   * does not wait for queued calls, so a transaction's body may read it:
   * before the COMMIT, it is the version that the commit would advance. It
   * waits for a statement that is running, and rejects once the session has
   * ended.
   */
  version() {
    return this.#native.version();
  }

  /**
   * A reader's: whether the store still holds the state it opened on, so that
   * what it read is. If not, read again, in a new session.
   */
  isCurrent() {
    return this.#serial(() => this.#native.isCurrent());
  }

  /** Whether SQL can still run: an SQL error leaves the session usable. */
  isUsable() {
    return this.#native.isUsable();
  }

  /**
   * End the session, after the calls before it, confirming its writes so that
   * the page cache may trust them.
   */
  save() {
    return this.#serial(() => this.#native.save());
  }

  /**
   * End the session without confirming: the next one in this process starts
   * cold. It does not wait for queued calls, which then fail.
   */
  close() {
    return this.#native.close();
  }
}

/**
 * Roll back the open transaction, unless a storage error ended the session.
 * The caller rethrows its own error, which says why: a failing rollback only
 * leaves the session unusable.
 */
async function rollback(native) {
  if (native.isUsable()) {
    await native.query('ROLLBACK').catch(() => {});
  }
}

/** SQL in one transaction, until it ends. */
class Transaction {
  #database;
  #native;
  // The transaction whose body this one's runs in, if any: of another
  // session, since a session's own are refused.
  #outer;
  /** See {@link recordCommit}. */
  recorded = false;

  constructor(database, nativeDatabase, outer) {
    this.#database = database;
    this.#native = nativeDatabase;
    this.#outer = outer;
  }

  /** Whether this transaction, or one it runs inside, is open and holds `database`. */
  holds(database) {
    return (this.#native !== null && this.#database === database) || (this.#outer?.holds(database) ?? false);
  }

  #check() {
    if (this.#native === null) throw new Error('the transaction has ended');
    return this.#native;
  }

  end() {
    this.#native = null;
  }

  /**
   * Log this transaction's commit even if it changes nothing (it only reads,
   * or its writes match no rows), so that the version grows when it commits.
   */
  recordCommit() {
    this.#check();
    this.recorded = true;
  }

  query(sql, params) {
    return this.#check().query(sql, params);
  }

  queryColumns(sql, params) {
    return this.#check().queryColumns(sql, params);
  }

  async execute(sql, params) {
    await this.query(sql, params);
  }
}

module.exports = { open, Database };

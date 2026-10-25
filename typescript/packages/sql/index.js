'use strict';
// SQL over a key/value store implemented in JavaScript. See index.d.ts.
const { AsyncLocalStorage } = require('node:async_hooks');
const addon = require('./sql.node');

const GET = 0;
const SET = 1;
const CLEAR = 2;
const SYNC = 3;
const KEYS = 4;

/**
 * Open the database of `namespace` over `store`. A `reader` reads beside the
 * namespace's owner, and writes nothing.
 */
async function open(namespace, store, { reader = false } = {}) {
  // A failed write cannot fail its caller, which has moved on: the next sync
  // reports it.
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
      case KEYS:
        // Every key, one per line: a sweep's (none where the store cannot
        // list its keys, and it is never swept).
        if (!store.keys) return Promise.resolve(null);
        return Promise.resolve(store.keys()).then((keys) => Buffer.from(keys.join('\n')));
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
      await native.begin();
      const tx = new Transaction(this, native, running.getStore());
      let result;
      try {
        result = await running.run(tx, () => body(tx));
      } catch (error) {
        tx.end();
        await rollback(native);
        throw error;
      }
      const effects = tx.effects;
      tx.end();
      try {
        await native.commit(effects);
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
   * The effects of the last commit that set them (see
   * `TransactionHandle.commitEffects`), as of the session's last commit; empty
   * if none did. It does not wait for queued calls, so a transaction's body
   * may read them. It waits for a statement that is running, and rejects once
   * the session has ended.
   */
  async effects() {
    return new Uint8Array(await this.#native.effects());
  }

  /** Whether SQL can still run: an SQL error leaves the session usable. */
  isUsable() {
    return this.#native.isUsable();
  }

  /** End the session, after the calls before it. */
  save() {
    return this.#serial(() => this.#native.save());
  }

  /** End the session now: queued calls fail. */
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
  await native.rollback().catch(() => {});
}

/** SQL in one transaction, until it ends. */
class Transaction {
  #database;
  #native;
  // The transaction whose body this one's runs in, if any: of another
  // session, since a session's own are refused.
  #outer;
  /** See {@link commitEffects}. */
  effects = undefined;

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
   * Commit `effects` with this transaction, in the same write: they are
   * stored exactly when it is, even if it changes nothing, and stay until a
   * later transaction commits others.
   */
  commitEffects(effects) {
    this.#check();
    this.effects = Buffer.from(effects);
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

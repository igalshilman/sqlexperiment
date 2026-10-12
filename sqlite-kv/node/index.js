'use strict';
// SQL over a key/value store implemented in JavaScript. See index.d.ts.
const native = require('./sqlite_kv.node');

const GET = 0;
const SET = 1;
const CLEAR = 2;
const SYNC = 3;

/**
 * Open the database of `namespace` over `store`. Every store call goes through
 * one queue, so the store sees them in the order they were issued.
 */
async function open(namespace, store) {
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
      default:
        return Promise.reject(new Error(`unknown store operation ${op}`));
    }
  };
  return new Database(await native.open(namespace, dispatch));
}

class Database {
  #native;

  constructor(nativeDatabase) {
    this.#native = nativeDatabase;
  }

  /** Run SQL and return its rows; parameters bind to a single statement. */
  query(sql, params) {
    return this.#native.query(sql, params);
  }

  /** Run SQL and return its rows with the column names of their statement. */
  queryColumns(sql, params) {
    return this.#native.queryColumns(sql, params);
  }

  /** Run SQL, discarding rows. */
  async execute(sql, params) {
    await this.#native.query(sql, params);
  }

  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, and
   * rolls back when it rejects.
   */
  async transaction(body) {
    await this.execute('BEGIN');
    let result;
    try {
      result = await body(this);
    } catch (error) {
      if (this.isUsable()) {
        await this.execute('ROLLBACK');
      }
      throw error;
    }
    await this.execute('COMMIT');
    return result;
  }

  /** Whether SQL can still run: an SQL error leaves the session usable. */
  isUsable() {
    return this.#native.isUsable();
  }

  /** End the session, confirming its writes so that the page cache may trust them. */
  save() {
    return this.#native.save();
  }

  /** End the session without confirming: the next one in this process starts cold. */
  close() {
    return this.#native.close();
  }
}

module.exports = { open, Database };

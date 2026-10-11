// pi-durable storage on a sqlite-kv database: the `SqliteDatabase` facade of
// @earendil-works/pi-durable/storage/sqlite, over a database session `db`
// (in Restate, the `db` of `ctx.sqlSession`), which has `query`,
// `queryColumns`, `execute` and `transaction(body)`.
//
// Operations run one at a time, in call order, as the facade requires. A
// statement outside a transaction is its own transaction.
import { SqliteStorage } from '@earendil-works/pi-durable/storage/sqlite';

/** Runs operations one at a time in call order. */
class SerialQueue {
  tail = Promise.resolve();
  run(operation) {
    const result = this.tail.then(operation);
    this.tail = result.catch(() => {});
    return result;
  }
}

const objects = ({ columns, rows }) =>
  rows.map((row) => Object.fromEntries(columns.map((column, i) => [column, row[i]])));

/** SQL through one transaction's `db`. */
class Executor {
  constructor(db) {
    this.db = db;
  }
  check() {}
  async exec(sql) {
    this.check();
    await this.db.execute(sql);
  }
  async run(sql, ...params) {
    this.check();
    await this.db.execute(sql, params);
  }
  async get(sql, ...params) {
    return (await this.all(sql, ...params))[0];
  }
  async all(sql, ...params) {
    this.check();
    return objects(await this.db.queryColumns(sql, params));
  }
}

/** The handle a transaction callback receives; invalid once the callback settles. */
class Transaction extends Executor {
  active = true;
  check() {
    if (!this.active) throw new Error('SQLite transaction handle is no longer active');
  }
}

/** The `SqliteDatabase` facade over the session `db`. */
export class SqlKvDatabase {
  #queue = new SerialQueue();
  #db;

  constructor(db) {
    this.#db = db;
  }

  #one(operation) {
    return this.#queue.run(() => operation(new Executor(this.#db)));
  }

  exec(sql) {
    return this.#one((executor) => executor.exec(sql));
  }
  run(sql, ...params) {
    return this.#one((executor) => executor.run(sql, ...params));
  }
  get(sql, ...params) {
    return this.#one((executor) => executor.get(sql, ...params));
  }
  all(sql, ...params) {
    return this.#one((executor) => executor.all(sql, ...params));
  }

  /** Commits when the callback resolves; rolls back, and rejects with its error, when it rejects. */
  transaction(callback) {
    return this.#queue.run(() =>
      this.#db.transaction(async (db) => {
        const transaction = new Transaction(db);
        try {
          return await callback(transaction);
        } finally {
          transaction.active = false;
        }
      }),
    );
  }

  /** Waits for queued work; the session is the caller's to end. */
  close() {
    return this.#queue.run(() => {});
  }
}

/** Open pi-durable storage on the session `db`. */
export function openSqlKvStorage(db) {
  return SqliteStorage.open(new SqlKvDatabase(db));
}

// pi-durable storage on a sqlite-kv database: the `SqliteDatabase` facade of
// @earendil-works/pi-durable/storage/sqlite, over a database session `db`
// (in Restate, the `db` of `ctx.sqlSession`), which has `query`,
// `queryColumns`, `execute` and `transaction(body)`.
//
// Operations run one at a time, in call order, as the facade requires. A
// statement outside a transaction is its own transaction.
import type { SqlDatabase, SqlSession } from '@restatedev/restate-sdk';
import {
  SqliteStorage,
  type SqliteDatabase,
  type SqliteExecutor,
  type SqliteValue,
} from '@earendil-works/pi-durable/storage/sqlite';

/** Runs operations one at a time in call order. */
class SerialQueue {
  tail: Promise<unknown> = Promise.resolve();
  run<T>(operation: () => T | Promise<T>): Promise<T> {
    const result = this.tail.then(operation);
    this.tail = result.catch(() => {});
    return result;
  }
}

const objects = <T>({ columns, rows }: { columns: string[]; rows: unknown[][] }): T[] =>
  rows.map((row) => Object.fromEntries(columns.map((column, i) => [column, row[i]])) as T);

/** SQL through one transaction's `db`. */
class Executor implements SqliteExecutor {
  readonly db: SqlDatabase;
  constructor(db: SqlDatabase) {
    this.db = db;
  }
  check() {}
  async exec(sql: string) {
    this.check();
    await this.db.execute(sql);
  }
  async run(sql: string, ...params: SqliteValue[]) {
    this.check();
    await this.db.execute(sql, params);
  }
  async get<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T | undefined> {
    return (await this.all<T>(sql, ...params))[0];
  }
  async all<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T[]> {
    this.check();
    return objects<T>(await this.db.queryColumns(sql, params));
  }
}

/** The handle a transaction callback receives; invalid once the callback settles. */
class Transaction extends Executor {
  active = true;
  override check() {
    if (!this.active) throw new Error('SQLite transaction handle is no longer active');
  }
}

/** The `SqliteDatabase` facade over the session `db`. */
export class SqlKvDatabase implements SqliteDatabase {
  #queue = new SerialQueue();
  #db: SqlSession;

  constructor(db: SqlSession) {
    this.#db = db;
  }

  #one<T>(operation: (executor: Executor) => Promise<T>): Promise<T> {
    return this.#queue.run(() => operation(new Executor(this.#db)));
  }

  exec(sql: string) {
    return this.#one((executor) => executor.exec(sql));
  }
  run(sql: string, ...params: SqliteValue[]) {
    return this.#one((executor) => executor.run(sql, ...params));
  }
  get<T extends object>(sql: string, ...params: SqliteValue[]) {
    return this.#one((executor) => executor.get<T>(sql, ...params));
  }
  all<T extends object>(sql: string, ...params: SqliteValue[]) {
    return this.#one((executor) => executor.all<T>(sql, ...params));
  }

  /** Commits when the callback resolves; rolls back, and rejects with its error, when it rejects. */
  transaction<T>(callback: (transaction: SqliteExecutor) => Promise<T>): Promise<T> {
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
export function openSqlKvStorage(db: SqlSession): Promise<SqliteStorage> {
  return SqliteStorage.open(new SqlKvDatabase(db));
}

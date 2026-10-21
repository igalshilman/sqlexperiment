// pi-durable storage on a sqlite-kv database: the `SqliteDatabase` facade of
// @earendil-works/pi-durable/storage/sqlite, over a database session `db`
// (in Restate, `ctx.db`), which has `query`,
// `queryColumns`, `execute` and `transaction(body)`.
//
// The facade's operations must run one at a time, a transaction excluding
// every other: the session does that, and a transaction's handle is invalid
// once its body settles. A statement outside a transaction is its own
// transaction.
import type { SqlDatabase, SqlSession } from 'restate-fns';
import {
  SqliteStorage,
  type SqliteDatabase,
  type SqliteExecutor,
  type SqliteValue,
} from '@earendil-works/pi-durable/storage/sqlite';

const objects = <T>({ columns, rows }: { columns: string[]; rows: unknown[][] }): T[] =>
  rows.map((row) => Object.fromEntries(columns.map((column, i) => [column, row[i]])) as T);

/** SQL through `db`: the session, or one transaction's. */
class Executor implements SqliteExecutor {
  readonly db: SqlDatabase;
  constructor(db: SqlDatabase) {
    this.db = db;
  }
  async exec(sql: string) {
    await this.db.execute(sql);
  }
  async run(sql: string, ...params: SqliteValue[]) {
    await this.db.execute(sql, params);
  }
  async get<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T | undefined> {
    return (await this.all<T>(sql, ...params))[0];
  }
  async all<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T[]> {
    return objects<T>(await this.db.queryColumns(sql, params));
  }
}

/** The `SqliteDatabase` facade over the session `db`. */
export class SqlKvDatabase extends Executor implements SqliteDatabase {
  readonly #session: SqlSession;

  constructor(db: SqlSession) {
    super(db);
    this.#session = db;
  }

  /** Commits when the callback resolves; rolls back, and rejects with its error, when it rejects. */
  transaction<T>(callback: (transaction: SqliteExecutor) => Promise<T>): Promise<T> {
    return this.#session.transaction((tx) => callback(new Executor(tx)));
  }

  /** The session is the caller's to end, after the calls made before. */
  async close() {}
}

/** Open pi-durable storage on the session `db`. */
export function openSqlKvStorage(db: SqlSession): Promise<SqliteStorage> {
  return SqliteStorage.open(new SqlKvDatabase(db));
}

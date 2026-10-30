// pi-durable storage on an SQL database: the `SqliteDatabase` facade of
// @earendil-works/pi-durable/storage/sqlite, over a database session `db`
// (in Restate, `ctx.db`), which has `query`, `queryColumns`, `execute` and
// `transaction(body)`.
//
// The facade's operations must run one at a time, a transaction excluding
// every other: the session does that, and a transaction's handle is invalid
// once its body settles. A statement outside a transaction is its own
// transaction.
//
// pi-durable names its tables and indexes plainly (`tasks`, `entries`...):
// each statement here has them renamed `restate_pi_<name>`, apart from the
// tables of yours in the same database. Its SQL is fixed text, where those
// names are only ever tables' and indexes' (storage/sqlite of 1.1.0).
import type { SqlDatabase, SqlSession } from 'restate-objects';
import {
  SqliteStorage,
  type SqliteDatabase,
  type SqliteExecutor,
  type SqliteValue,
} from '@earendil-works/pi-durable/storage/sqlite';

// pi-durable's tables and indexes (storage/sqlite/migrations.js of 1.1.0).
const NAMES = [
  'conversations',
  'conversations_by_owner_conversation',
  'conversations_by_owner_task',
  'document_revisions',
  'document_revisions_by_kind',
  'documents',
  'documents_by_address',
  'documents_by_scope',
  'documents_by_scope_kind',
  'durable_metadata',
  'durable_schema',
  'entries',
  'entries_by_conversation',
  'entry_heads_by_conversation',
  'record_ids',
  'submissions',
  'submissions_by_conversation',
  'submissions_by_request',
  'submissions_by_status',
  'tasks',
  'tasks_by_abort_requested',
  'tasks_by_background',
  'tasks_by_conversation',
  'tasks_by_kind',
  'tasks_by_status',
];
const NAMED = new RegExp(`\\b(${NAMES.join('|')})\\b`, 'g');

/** pi-durable's `sql`, its tables and indexes renamed `restate_pi_<name>`. */
const renamed = (sql: string) => sql.replace(NAMED, 'restate_pi_$1');

const objects = <T>({ columns, rows }: { columns: string[]; rows: unknown[][] }): T[] =>
  rows.map((row) => Object.fromEntries(columns.map((column, i) => [column, row[i]])) as T);

/** SQL through `db`: the session, or one transaction's. */
class Executor implements SqliteExecutor {
  readonly db: SqlDatabase;
  constructor(db: SqlDatabase) {
    this.db = db;
  }
  async exec(sql: string) {
    await this.db.execute(renamed(sql));
  }
  async run(sql: string, ...params: SqliteValue[]) {
    await this.db.execute(renamed(sql), params);
  }
  async get<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T | undefined> {
    return (await this.all<T>(sql, ...params))[0];
  }
  async all<T extends object>(sql: string, ...params: SqliteValue[]): Promise<T[]> {
    return objects<T>(await this.db.queryColumns(renamed(sql), params));
  }
}

/** The `SqliteDatabase` facade over the session `db`. */
class SqlKvDatabase extends Executor implements SqliteDatabase {
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

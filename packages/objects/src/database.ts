// A key's database, through the addon (`native/`): SQLite-compatible SQL,
// kept in the object's state. The addon reads and writes the state through a
// store (`objectStore` in session.ts), in the order it makes its requests
// (`native/src/node.rs`); this module orders the calls and runs transactions.
import { AsyncLocalStorage } from 'node:async_hooks';
import { Buffer } from 'node:buffer';
import { createRequire } from 'node:module';
import { PLATFORMS, platform } from './platform.ts';

/** An SQL value. Integers come back as numbers, or as bigints beyond 2^53. */
export type SqlValue = null | number | bigint | string | Uint8Array;

/** An SQL parameter: an `SqlValue`, or a boolean, stored as 0 or 1. */
export type SqlParam = SqlValue | boolean;

/** The rows of a statement, with its column names. */
export interface SqlQueryResult {
  columns: string[];
  rows: SqlValue[][];
}

/**
 * A failure a retry would repeat: SQL the engine rejected (a syntax error, a
 * constraint, a missing table), which undid just that statement and leaves
 * the session usable; a parameter it cannot bind; or a call the session
 * refused, which may have ended it (a `COMMIT` in a transaction's body, a
 * transaction left open). A storage error, which ends the session, rejects
 * as it is.
 */
export class SqlError extends Error {
  override name = 'SqlError';
}

/** A row as an object, by column name. */
export type Row = Record<string, SqlValue>;

/** Reads of a database, or of one transaction of it. */
export interface DatabaseReader {
  /**
   * The rows of one statement, each an object by column name; parameters
   * bind to a single statement. `T` declares the row's shape, unchecked:
   * integers past 2^53 come back as bigints, and booleans as 0 or 1. Columns
   * are named as SQL names them (an expression needs `AS`), and two of the
   * same name are an {@link SqlError}: alias them, or use {@link values}.
   */
  query<T = Row>(sql: string, params?: SqlParam[]): Promise<T[]>;
  /** The first row of {@link query}, or `undefined` when there is none. */
  get<T = Row>(sql: string, params?: SqlParam[]): Promise<T | undefined>;
  /** The rows of one statement as arrays, in column order: for adapters, and for columns that share a name. */
  values(sql: string, params?: SqlParam[]): Promise<SqlValue[][]>;
}

/** SQL: a database, or one transaction of it. */
export interface DatabaseWriter extends DatabaseReader {
  /** Run SQL (one or more statements), discarding rows; parameters only with a single statement. */
  execute(sql: string, params?: SqlParam[]): Promise<void>;
}

/** The rows of `result` as objects by column name; a column name twice is an {@link SqlError}. */
function objects({ columns, rows }: SqlQueryResult): Row[] {
  if (new Set(columns).size !== columns.length) {
    const twice = columns.find((name, i) => columns.indexOf(name) !== i);
    throw new SqlError(`the column ${twice} is named twice: alias one (AS), or read the rows with values()`);
  }
  return rows.map((row) => Object.fromEntries(columns.map((name, i) => [name, row[i]!])));
}

/** {@link DatabaseWriter} over `run`, which runs one call's SQL and returns its rows with their columns. */
export function sqlOver(run: (sql: string, params?: SqlParam[]) => Promise<SqlQueryResult>): DatabaseWriter {
  return {
    query: async <T>(sql: string, params?: SqlParam[]) => objects(await run(sql, params)) as T[],
    get: async <T>(sql: string, params?: SqlParam[]) => objects(await run(sql, params))[0] as T | undefined,
    values: async (sql, params) => (await run(sql, params)).rows,
    execute: async (sql, params) => void (await run(sql, params)),
  };
}

/**
 * A key/value store, the object's state: `set` and `clear` return at once,
 * and `sync` resolves once every write before it is stored. Writes are
 * stored in the order they were made: a crash keeps a prefix of the writes
 * since the last sync and loses the rest. `get` returns the stored value, or
 * this request's own latest write; `undefined` or `null` when absent.
 */
export interface Store {
  get(key: string): Promise<Uint8Array | null | undefined>;
  set(key: string, value: Uint8Array): void;
  clear(key: string): void;
  sync(): Promise<void>;
  /**
   * Every key, as `ctx.stateKeys()`: then a commit that checkpoints deletes
   * the packs no checkpoint needs any more. A store without it keeps them.
   */
  keys?(): Promise<string[]>;
}

/** The addon (`native/src/node.rs`). */
interface Addon {
  open(namespace: string, store: Store, reader: boolean): Promise<NativeSession>;
}

/**
 * A session of the addon: one call at a time holds it. A transaction's body
 * runs between `begin` and `commit` (or `rollback`). A call answers at once
 * with its value (or throws) when it needs nothing from the store, such as
 * SQL on cached pages, and with a promise when it waits for the store.
 */
interface NativeSession {
  queryColumns(sql: string, params?: SqlParam[]): Answer<SqlQueryResult>;
  begin(): Answer<void>;
  commit(records: Record<string, Buffer>): Answer<void>;
  rollback(): Answer<void>;
  record(name: string): Answer<Buffer>;
  isUsable(): boolean;
  save(): Answer<void>;
  close(): Promise<void>;
}

/** A value, or a promise of it: `await` takes either. */
type Answer<T> = T | Promise<T>;

const addon = load();

/** The addon of this machine's platform, from its package (see platform.ts). */
function load(): Addon {
  const machine = platform();
  const name = `restate-objects-${machine}`;
  try {
    return createRequire(import.meta.url)(name) as Addon;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'MODULE_NOT_FOUND') throw error;
    throw new Error(
      `restate-objects found no addon for ${machine}, the package ${name}: it ships them for ` +
        `${PLATFORMS.join(', ')}. In its repository, \`pnpm build\` builds this machine's.`,
      { cause: error },
    );
  }
}

/**
 * Open the database of `namespace` over `store`. One process-wide page cache
 * serves every namespace. A `reader` reads beside the namespace's owner (a
 * shared handler), the state as of open, and writes nothing.
 */
export async function open(namespace: string, store: Store, { reader = false } = {}): Promise<Connection> {
  return new Connection(await addon.open(namespace, store, reader));
}

/**
 * SQL on a session of the addon: {@link DatabaseWriter}'s `query` and `execute`
 * by way of its `queryColumns`.
 */
abstract class Sql implements DatabaseWriter {
  protected readonly native: NativeSession;

  constructor(native: NativeSession) {
    this.native = native;
  }

  abstract queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult>;

  query<T = Row>(sql: string, params?: SqlParam[]): Promise<T[]> {
    return sqlOver((s, p) => this.queryColumns(s, p)).query<T>(sql, params);
  }

  get<T = Row>(sql: string, params?: SqlParam[]): Promise<T | undefined> {
    return sqlOver((s, p) => this.queryColumns(s, p)).get<T>(sql, params);
  }

  values(sql: string, params?: SqlParam[]): Promise<SqlValue[][]> {
    return sqlOver((s, p) => this.queryColumns(s, p)).values(sql, params);
  }

  async execute(sql: string, params?: SqlParam[]): Promise<void> {
    await this.queryColumns(sql, params);
  }

  /**
   * `call`'s result, or its {@link failure}: `call` makes the request, as the
   * addon throws as it is called (arguments it cannot convert, and the error
   * of a call that answers at once).
   */
  protected async request<T>(call: () => Answer<T>): Promise<T> {
    try {
      return await call();
    } catch (error) {
      throw this.failure(error);
    }
  }

  /** A request's `error`: an {@link SqlError} if a retry would repeat it. */
  protected failure(error: unknown): unknown {
    if (error instanceof SqlError) return error;
    const repeats = (error as { code?: unknown }).code === REFUSED || this.native.isUsable();
    return repeats ? new SqlError((error as Error).message, { cause: error }) : error;
  }
}

// The code of the addon's errors that a retry would repeat even though they
// ended the session (its `Refused`), and of arguments it cannot convert.
const REFUSED = 'InvalidArg';

// The transaction whose body is running, in that body's asynchronous context:
// the innermost, which knows the ones it runs inside.
const running = new AsyncLocalStorage<OpenTransaction>();

/**
 * A session of a database: calls run one at a time, in call order, and a
 * transaction holds the session until it commits or rolls back. Calls made
 * meanwhile wait, but a call from inside the transaction's body is refused,
 * as it would wait for that body: the body uses its `tx`.
 */
export class Connection extends Sql {
  /** Settles once the last call queued so far has: the next call runs after it. */
  private queue: Promise<unknown> = Promise.resolve();

  queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult> {
    return this.serial(() => this.request(() => this.native.queryColumns(sql, params)));
  }

  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, with
   * the records it set, and rolls back when it rejects, with the body's own
   * error, once the calls it made have settled. A statement in it that ends
   * the transaction (`COMMIT`, `ROLLBACK`) ends the session. A failing
   * COMMIT rolls back.
   */
  transaction<T>(body: (tx: ConnectionTransaction) => Promise<T>): Promise<T> {
    return this.serial(async () => {
      await this.request(() => this.native.begin());
      const tx = new OpenTransaction(this, this.native, running.getStore());
      let result: T;
      try {
        result = await running.run(tx, () => body(tx));
      } catch (error) {
        await tx.end();
        await this.rollback();
        throw error;
      }
      const records = await tx.end();
      try {
        await this.native.commit(records);
      } catch (error) {
        // A COMMIT that fails (a deferred constraint) leaves the transaction
        // open: without the rollback, the calls after it would run inside it.
        await this.rollback();
        throw this.failure(error);
      }
      return result;
    });
  }

  /**
   * The record `name`, as the last commit that set it left it (see
   * {@link ConnectionTransaction.setRecord}); empty if none did.
   */
  record(name: string): Promise<Uint8Array> {
    return this.serial(async () => new Uint8Array(await this.request(() => this.native.record(name))));
  }

  /** Whether SQL can still run: an SQL error leaves the session usable, a storage error ends it. */
  isUsable(): boolean {
    return this.native.isUsable();
  }

  /**
   * End the session after the calls before it, leaving its engine to the
   * next one. It rejects if they left a transaction open, or a failure ended
   * the session.
   */
  save(): Promise<void> {
    return this.serial(() => this.request(() => this.native.save()));
  }

  /** End the session now, whatever its state: queued calls reject. */
  close(): Promise<void> {
    return this.native.close();
  }

  /** Run `call` once the calls before it have, unless it comes from the body of a transaction it would wait for. */
  private serial<T>(call: () => Promise<T>): Promise<T> {
    if (running.getStore()?.holds(this)) {
      return Promise.reject(new SqlError('inside a transaction, use the transaction handle its body gets'));
    }
    const result = this.queue.then(call);
    this.queue = result.catch(() => {});
    return result;
  }

  /** Roll back, unless a storage error ended the session: the caller rethrows its own error, which says why. */
  private async rollback(): Promise<void> {
    try {
      await this.native.rollback();
    } catch {}
  }
}

/** SQL in one transaction, while its body runs, and the records it commits with. */
export interface ConnectionTransaction extends DatabaseWriter {
  /** The rows of one statement, with their column names. */
  queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult>;
  /**
   * Set the record `name` to `value` with this transaction, in the same
   * write: a whole value kept beside the SQL, in no table, stored exactly
   * when the transaction is, even if it changes no row. It stays until a
   * later transaction sets it again; an empty value reads as never set. A
   * name is at most 255 bytes, and not `db`; a value at most 16 MiB.
   */
  setRecord(name: string, value: Uint8Array): void;
  /** The record `name` as this transaction would commit it: as it set it, or as committed. */
  record(name: string): Promise<Uint8Array>;
}

/**
 * A {@link ConnectionTransaction} of {@link Connection.transaction}, open until its body
 * settles. Its calls run one at a time, in call order, also when the body
 * awaits several together, or none: each call made while it is open runs
 * before its commit or rollback, so none outlives it.
 */
class OpenTransaction extends Sql implements ConnectionTransaction {
  private readonly database: Connection;
  /** The transaction whose body this one's runs in, if any: of another database, since a database's own are refused. */
  private readonly outer: OpenTransaction | undefined;
  /** The records it sets, by name. */
  private readonly records = new Map<string, Buffer>();
  /** Settles once the last call queued so far has: the next call runs after it. */
  private queue: Promise<unknown> = Promise.resolve();
  private ended = false;

  constructor(database: Connection, native: NativeSession, outer: OpenTransaction | undefined) {
    super(native);
    this.database = database;
    this.outer = outer;
  }

  /** Whether this transaction, or one it runs inside, is open and holds `database`. */
  holds(database: Connection): boolean {
    return (!this.ended && this.database === database) || (this.outer?.holds(database) ?? false);
  }

  /**
   * End it, refusing calls from now on, once the calls made before have
   * settled; then the records it set, as the addon's commit takes them.
   */
  async end(): Promise<Record<string, Buffer>> {
    this.ended = true;
    await this.queue;
    return Object.fromEntries(this.records);
  }

  queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult> {
    return this.serial(() => this.request(() => this.native.queryColumns(sql, params)));
  }

  setRecord(name: string, value: Uint8Array): void {
    this.refuseEnded();
    this.records.set(name, Buffer.from(value));
  }

  record(name: string): Promise<Uint8Array> {
    // As set when it is called, in call order with `setRecord`.
    const set = this.records.get(name);
    return this.serial(async () => new Uint8Array(set ?? (await this.request(() => this.native.record(name)))));
  }

  /** Run `call`, made while the transaction is open, once the calls before it have. */
  private serial<T>(call: () => Promise<T>): Promise<T> {
    if (this.ended) return Promise.reject(new SqlError('the transaction has ended'));
    const result = this.queue.then(call);
    this.queue = result.catch(() => {});
    return result;
  }

  private refuseEnded(): void {
    if (this.ended) throw new SqlError('the transaction has ended');
  }
}

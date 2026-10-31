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

/** SQL: a database, or one transaction of it. */
export interface SqlDatabase {
  /** Run SQL and return its rows; parameters bind to a single statement. */
  query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]>;
  /** {@link query}, with the column names of the statement whose rows these are. */
  queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult>;
  /** Run SQL (one or more statements), discarding rows. */
  execute(sql: string, params?: SqlParam[]): Promise<void>;
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
 * runs between `begin` and `commit` (or `rollback`).
 */
interface NativeSession {
  queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult>;
  begin(): Promise<void>;
  commit(records: Record<string, Buffer>): Promise<void>;
  rollback(): Promise<void>;
  record(name: string): Promise<Buffer>;
  isUsable(): boolean;
  save(): Promise<void>;
  close(): Promise<void>;
}

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
export async function open(namespace: string, store: Store, { reader = false } = {}): Promise<Database> {
  return new Database(await addon.open(namespace, store, reader));
}

/**
 * SQL on a session of the addon: {@link SqlDatabase}'s `query` and `execute`
 * by way of its `queryColumns`.
 */
abstract class Sql implements SqlDatabase {
  protected readonly native: NativeSession;

  constructor(native: NativeSession) {
    this.native = native;
  }

  abstract queryColumns(sql: string, params?: SqlParam[]): Promise<SqlQueryResult>;

  async query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]> {
    return (await this.queryColumns(sql, params)).rows;
  }

  async execute(sql: string, params?: SqlParam[]): Promise<void> {
    await this.queryColumns(sql, params);
  }

  /**
   * `call`'s result, or its {@link failure}: `call` makes the request, as the
   * addon throws arguments it cannot convert as it is called.
   */
  protected async request<T>(call: () => Promise<T>): Promise<T> {
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
export class Database extends Sql {
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
  transaction<T>(body: (tx: Transaction) => Promise<T>): Promise<T> {
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
   * {@link Transaction.setRecord}); empty if none did.
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
    await this.native.rollback().catch(() => {});
  }
}

/** SQL in one transaction, while its body runs, and the records it commits with. */
export interface Transaction extends SqlDatabase {
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
 * A {@link Transaction} of {@link Database.transaction}, open until its body
 * settles. Its calls run one at a time, in call order, also when the body
 * awaits several together, or none: each call made while it is open runs
 * before its commit or rollback, so none outlives it.
 */
class OpenTransaction extends Sql implements Transaction {
  private readonly database: Database;
  /** The transaction whose body this one's runs in, if any: of another database, since a database's own are refused. */
  private readonly outer: OpenTransaction | undefined;
  /** The records it sets, by name. */
  private readonly records = new Map<string, Buffer>();
  /** Settles once the last call queued so far has: the next call runs after it. */
  private queue: Promise<unknown> = Promise.resolve();
  private ended = false;

  constructor(database: Database, native: NativeSession, outer: OpenTransaction | undefined) {
    super(native);
    this.database = database;
    this.outer = outer;
  }

  /** Whether this transaction, or one it runs inside, is open and holds `database`. */
  holds(database: Database): boolean {
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
    return this.serial(async () => {
      const set = this.records.get(name) ?? (await this.request(() => this.native.record(name)));
      return new Uint8Array(set);
    });
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

// A key's database, through the addon (`native/`): SQLite-compatible SQL,
// kept in the object's state. The addon reads and writes the state through a
// store (`objectStore` in session.ts); this module orders the calls and runs
// transactions.
import { AsyncLocalStorage } from 'node:async_hooks';
import { Buffer } from 'node:buffer';
import { createRequire } from 'node:module';
import { PLATFORMS, platform } from './platform.ts';

/** An SQL value. Integers come back as numbers, or as bigints beyond 2^53. */
export type SqlValue = null | number | bigint | string | Uint8Array;

/** An SQL parameter: an `SqlValue`, or a boolean, stored as 0 or 1. */
export type SqlParam = SqlValue | boolean;

/** Rows with the column names of their statement. */
export type Columns = { columns: string[]; rows: SqlValue[][] };

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

/** The addon's session (`native/src/node.rs`). */
interface Native {
  queryColumns(sql: string, params?: SqlParam[]): Promise<Columns>;
  begin(): Promise<void>;
  commit(records?: Record<string, Buffer>): Promise<void>;
  rollback(): Promise<void>;
  record(name: string): Promise<Buffer>;
  isUsable(): boolean;
  save(): Promise<void>;
  close(): Promise<void>;
}

type Request = [op: number, key: string, value: Uint8Array | null];

const addon = load() as {
  open(namespace: string, dispatch: (request: Request) => unknown, reader: boolean): Promise<Native>;
};

/** The addon of this machine's platform: its package's (see platform.ts). */
function load(): unknown {
  const name = `restate-objects-${platform()}`;
  try {
    return createRequire(import.meta.url)(name);
  } catch (error) {
    if ((error as { code?: unknown }).code !== 'MODULE_NOT_FOUND') throw error;
    throw new Error(
      `restate-objects found no addon for ${platform()}, the package ${name}: it ships them for ` +
        `${PLATFORMS.join(', ')}. In its repository, \`pnpm build\` builds this machine's.`,
      { cause: error },
    );
  }
}

// The store's operations, as the addon passes them.
const GET = 0;
const SET = 1;
const CLEAR = 2;
const SYNC = 3;
const KEYS = 4;

/**
 * Open the database of `namespace` over `store`. One process-wide page cache
 * serves every namespace. A `reader` reads beside the namespace's owner (a
 * shared handler), the state as of open, and writes nothing.
 */
export async function open(namespace: string, store: Store, { reader = false } = {}): Promise<Database> {
  // A failed write cannot fail its caller, which has moved on: the next sync
  // reports it.
  let failure: unknown;
  const dispatch = ([op, key, value]: Request) => {
    switch (op) {
      case GET:
        return store.get(key).then((v) => v ?? null);
      case SET:
      case CLEAR:
        try {
          if (op === SET) store.set(key, value!);
          else store.clear(key);
        } catch (error) {
          failure ??= error;
        }
        return undefined;
      case SYNC:
        if (failure !== undefined) return Promise.reject(failure);
        return store.sync().then(() => null);
      case KEYS:
        // Every key, one per line; none where the store cannot list them.
        if (!store.keys) return Promise.resolve(null);
        return store.keys().then((keys) => Buffer.from(keys.join('\n')));
      default:
        return Promise.reject(new Error(`unknown store operation ${op}`));
    }
  };
  return new Database(await addon.open(namespace, dispatch, reader));
}

// The transaction whose body is running, in that body's asynchronous context:
// the innermost, which knows the ones it runs inside.
const running = new AsyncLocalStorage<Transaction>();

/**
 * A session of a database: calls run one at a time, in call order, and a
 * transaction holds the session until it commits or rolls back. Calls made
 * meanwhile wait, but a call from inside the transaction's body is refused,
 * as it would wait for that body: the body uses its `tx`.
 */
export class Database {
  readonly #native: Native;
  #tail: Promise<unknown> = Promise.resolve();

  constructor(native: Native) {
    this.#native = native;
  }

  #serial<T>(operation: () => Promise<T>): Promise<T> {
    if (running.getStore()?.holds(this)) {
      return Promise.reject(new Error('inside a transaction, use the transaction handle its body gets'));
    }
    const result = this.#tail.then(operation);
    this.#tail = result.catch(() => {});
    return result;
  }

  /** Run SQL and return its rows; parameters bind to a single statement. */
  async query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]> {
    return (await this.queryColumns(sql, params)).rows;
  }

  /** {@link query}, with the column names of the statement whose rows these are. */
  queryColumns(sql: string, params?: SqlParam[]): Promise<Columns> {
    return this.#serial(() => this.#native.queryColumns(sql, params));
  }

  /** Run SQL (one or more statements), discarding rows. */
  async execute(sql: string, params?: SqlParam[]): Promise<void> {
    await this.queryColumns(sql, params);
  }

  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, and
   * rolls back when it rejects. A statement in it that ends the transaction
   * (`COMMIT`, `ROLLBACK`) ends the session. A failing COMMIT rolls back.
   */
  transaction<T>(body: (tx: Transaction) => Promise<T>): Promise<T> {
    return this.#serial(async () => {
      const native = this.#native;
      await native.begin();
      const tx = new Transaction(this, native, running.getStore());
      let result: T;
      try {
        result = await running.run(tx, () => body(tx));
      } catch (error) {
        tx.end();
        await rollback(native);
        throw error;
      }
      const records = tx.records();
      tx.end();
      try {
        await native.commit(records);
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
   * The record `name` as this session holds it: as the last commit that set
   * it ({@link Transaction.setRecord}) left it; empty if none did. It does
   * not wait for queued calls, so a transaction's body may read it.
   */
  async record(name: string): Promise<Uint8Array> {
    return new Uint8Array(await this.#native.record(name));
  }

  /** Whether SQL can still run: an SQL error leaves the session usable, a storage error ends it. */
  isUsable(): boolean {
    return this.#native.isUsable();
  }

  /**
   * End the session after the calls before it, leaving its engine to the
   * next one. It rejects if they left a transaction open, or a failure ended
   * the session.
   */
  save(): Promise<void> {
    return this.#serial(() => this.#native.save());
  }

  /** End the session now, whatever its state: queued calls reject. */
  close(): Promise<void> {
    return this.#native.close();
  }
}

/** Roll back, unless a storage error ended the session: the caller rethrows its own error, which says why. */
async function rollback(native: Native) {
  await native.rollback().catch(() => {});
}

/** SQL in one transaction, until it ends. */
export class Transaction {
  readonly #database: Database;
  #native: Native | null;
  // The transaction whose body this one's runs in, if any: of another
  // database, since a database's own are refused.
  readonly #outer: Transaction | undefined;
  // The records it sets, by name.
  readonly #records = new Map<string, Buffer>();

  constructor(database: Database, native: Native, outer: Transaction | undefined) {
    this.#database = database;
    this.#native = native;
    this.#outer = outer;
  }

  /** Whether this transaction, or one it runs inside, is open and holds `database`. */
  holds(database: Database): boolean {
    return (this.#native !== null && this.#database === database) || (this.#outer?.holds(database) ?? false);
  }

  #check(): Native {
    if (this.#native === null) throw new Error('the transaction has ended');
    return this.#native;
  }

  end() {
    this.#native = null;
  }

  /**
   * Set the record `name` to `value` with this transaction, in the same
   * write: a whole value kept beside the SQL, in no table, stored exactly
   * when the transaction is, even if it changes no row. It stays until a
   * later transaction sets it again; an empty value reads as never set (see
   * {@link Database.record}). A name is at most 255 bytes, and not `db`; a
   * value at most 16 MiB.
   */
  setRecord(name: string, value: Uint8Array) {
    this.#check();
    this.#records.set(name, Buffer.from(value));
  }

  /** The records it sets, as the addon's commit takes them. */
  records(): Record<string, Buffer> {
    return Object.fromEntries(this.#records);
  }

  async query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]> {
    return (await this.queryColumns(sql, params)).rows;
  }

  queryColumns(sql: string, params?: SqlParam[]): Promise<Columns> {
    return this.#check().queryColumns(sql, params);
  }

  async execute(sql: string, params?: SqlParam[]): Promise<void> {
    await this.queryColumns(sql, params);
  }
}

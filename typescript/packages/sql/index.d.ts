/** An SQL value. Integers come back as numbers, or as bigints beyond 2^53. */
export type SqlValue = null | number | bigint | string | Uint8Array | boolean;

/**
 * A key/value store: `set` and `clear` return at once, and `sync` resolves
 * once every write before it is stored. Writes are stored in no particular
 * order: until a sync resolves, any of them may be lost. `get` returns the
 * stored value, or this request's own latest write; `undefined` or `null`
 * when absent.
 */
export interface Store {
  get(key: string): Promise<Uint8Array | null | undefined> | Uint8Array | null | undefined;
  set(key: string, value: Uint8Array): void;
  clear(key: string): void;
  sync(): Promise<void> | void;
}

/** SQL in one transaction: invalid once the transaction's body settles. */
export interface Transaction {
  query(sql: string, params?: SqlValue[]): Promise<SqlValue[][]>;
  queryColumns(sql: string, params?: SqlValue[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  execute(sql: string, params?: SqlValue[]): Promise<void>;
}

/** The handle a transaction's body gets. */
export interface TransactionHandle extends Transaction {
  /**
   * Commit `effects` with this transaction, in the same write: they are
   * stored exactly when it is, even if it changes nothing, and stay until a
   * later transaction commits others (see `Database.effects`).
   */
  commitEffects(effects: Uint8Array): void;
}

/**
 * A session: calls run one at a time, in call order, and a transaction holds
 * the session until it commits or rolls back. Calls made meanwhile wait, but
 * a call to the session from inside the transaction's body is refused, as it
 * would wait for that body: the body uses its `tx`.
 */
export declare class Database implements Transaction {
  query(sql: string, params?: SqlValue[]): Promise<SqlValue[][]>;
  queryColumns(sql: string, params?: SqlValue[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  execute(sql: string, params?: SqlValue[]): Promise<void>;
  transaction<T>(body: (tx: TransactionHandle) => Promise<T>): Promise<T>;
  /**
   * The effects the last commit that set them committed (see
   * `TransactionHandle.commitEffects`); empty if none did. It does not wait
   * for queued calls, so a transaction's body may read them; it waits for a
   * statement that is running, and rejects once the session has ended.
   */
  effects(): Promise<Uint8Array>;
  isUsable(): boolean;
  /**
   * A reader's: whether the store still holds the state it opened on (no
   * commit since), so that what it read is. If not, read again, in a new
   * session.
   */
  isCurrent(): Promise<boolean>;
  save(): Promise<void>;
  close(): Promise<void>;
}

/**
 * Open the database of `namespace` over `store`. One process-wide page cache
 * serves every namespace. A statement that commits resolves once the store
 * has stored its writes (a `sync` after them resolved).
 */
export declare function open(namespace: string, store: Store, options?: OpenOptions): Promise<Database>;

export interface OpenOptions {
  /**
   * Read the namespace beside its owner, which writes meanwhile (a virtual
   * object's shared handler): without the page cache, and without writing to
   * the store; SQL changes stay in the session. What it reads is the state
   * as of open while `isCurrent()` says so.
   */
  reader?: boolean;
}

/** An SQL value. Integers come back as numbers, or as bigints beyond 2^53. */
export type SqlValue = null | number | bigint | string | Uint8Array;

/** An SQL parameter: an `SqlValue`, or a boolean, stored as 0 or 1. */
export type SqlParam = SqlValue | boolean;

/**
 * A key/value store: `set` and `clear` return at once, and `sync` resolves
 * once every write before it is stored. Writes are stored in the order they
 * were made: a crash keeps a prefix of the writes since the last sync and
 * loses the rest, never a write without the ones before it. `get` returns the stored value, or this
 * request's own latest write; `undefined` or `null` when absent.
 */
export interface Store {
  get(key: string): Promise<Uint8Array | null | undefined> | Uint8Array | null | undefined;
  set(key: string, value: Uint8Array): void;
  clear(key: string): void;
  sync(): Promise<void> | void;
  /**
   * Every key, as the object's `ctx.stateKeys()`: then a commit that
   * checkpoints deletes the checkpoints' packs no checkpoint needs any more.
   * A store without it keeps them.
   */
  keys?(): Promise<string[]> | string[];
}

/** SQL in one transaction: invalid once the transaction's body settles. */
export interface Transaction {
  query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]>;
  queryColumns(sql: string, params?: SqlParam[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  execute(sql: string, params?: SqlParam[]): Promise<void>;
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
  query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]>;
  queryColumns(sql: string, params?: SqlParam[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  execute(sql: string, params?: SqlParam[]): Promise<void>;
  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, and
   * rolls back when it rejects. A statement in it that ends the transaction
   * (`COMMIT`, `ROLLBACK`) ends the session.
   */
  transaction<T>(body: (tx: TransactionHandle) => Promise<T>): Promise<T>;
  /**
   * The effects of the last commit that set them (see
   * `TransactionHandle.commitEffects`); empty if none did. It does not wait
   * for queued calls, so a transaction's body may read them; it waits for a
   * statement that is running, and rejects once the session has ended.
   */
  effects(): Promise<Uint8Array>;
  /**
   * Whether SQL can still run: an SQL error leaves the session usable, a
   * storage error ends it.
   */
  isUsable(): boolean;
  /**
   * End the session after the calls before it, leaving its engine to the
   * next one. It rejects if they left a transaction open, whose changes are
   * lost, or a failure ended the session.
   */
  save(): Promise<void>;
  /** End the session now, whatever its state: queued calls reject. */
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
   * object's shared handler): from the packs the owner cached, writing
   * nothing to the store or the cache; SQL changes stay in the session. It
   * reads the state as of open, a snapshot, whatever the owner commits
   * meanwhile, until the owner's second checkpoint after it: a reader still
   * reading then may fail ("is gone"): open it again (a shared handler is
   * retried).
   */
  reader?: boolean;
}

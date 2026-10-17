/** An SQL value. Integers come back as numbers, or as bigints beyond 2^53. */
export type SqlValue = null | number | bigint | string | Uint8Array | boolean;

/**
 * A key/value store with ordered writes: `set` and `clear` return at once, the
 * store keeps writes in the order they were issued, and `sync` resolves once
 * every write so far is stored. `get` returns the stored value, or this
 * request's own latest write; `undefined` or `null` when absent.
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
   * Log this transaction's commit even if it changes nothing (it only reads,
   * or its writes match no rows), so that `version()` grows when it commits.
   */
  recordCommit(): void;
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
   * The database's version: it grows with each commit that changes the
   * database, or whose transaction called `recordCommit()`, and only then. It
   * does not wait for queued calls, so a transaction's body may read it; it
   * waits for a statement that is running, and rejects once the session has
   * ended.
   */
  version(): Promise<number>;
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
 * serves every namespace. Each SQL commit is durable once the store has stored
 * its writes: a statement resolves once they reach the store, in order, and
 * `save()` waits until they are stored.
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

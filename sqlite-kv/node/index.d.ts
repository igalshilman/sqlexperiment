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
  transaction<T>(body: (tx: Transaction) => Promise<T>): Promise<T>;
  /**
   * The database's version: it grows with each commit, and only then. It does
   * not wait for queued calls, so a transaction's body may read it.
   */
  version(): Promise<number>;
  isUsable(): boolean;
  save(): Promise<void>;
  close(): Promise<void>;
}

/**
 * Open the database of `namespace` over `store`. One process-wide page cache
 * serves every namespace. Each SQL commit is durable once the store has stored
 * its writes: a statement resolves once they reach the store, in order, and
 * `save()` waits until they are stored.
 */
export declare function open(namespace: string, store: Store): Promise<Database>;

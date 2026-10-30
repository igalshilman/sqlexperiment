// The one row a query returns, typed: an error when it returns none.
import type { SqlDatabase, SqlValue } from 'restate-objects';

/** The first row of `sql`'s result, as the tuple `T` it is known to be. */
export async function row<T extends SqlValue[]>(db: SqlDatabase, sql: string, params?: SqlValue[]): Promise<T> {
  const [first] = await db.query(sql, params);
  if (!first) throw new Error(`no row: ${sql}`);
  return first as T;
}

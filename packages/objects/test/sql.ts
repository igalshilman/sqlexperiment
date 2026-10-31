// The one row a query returns, typed: an error when it returns none.
import type { DatabaseWriter, SqlValue } from 'restate-objects';

/** The first row of `sql`'s result, as the tuple `T` it is known to be. */
export async function row<T extends SqlValue[]>(db: DatabaseWriter, sql: string, params?: SqlValue[]): Promise<T> {
  const [first] = await db.values(sql, params);
  if (!first) throw new Error(`no row: ${sql}`);
  return first as T;
}

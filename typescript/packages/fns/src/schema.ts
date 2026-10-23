// The schema of an object's databases: each key's database is made by the
// same steps, in order, applied once per key, where its first handler opens
// it. A database keeps how far it got in the table `restate_schema`, so a
// step added later (a column, an index) runs on each key's database once, as
// that key is next opened:
//
//   const notes = restate.object({
//     name: 'notes',
//     schema: ['CREATE TABLE notes (text TEXT NOT NULL)', 'ALTER TABLE notes ADD COLUMN at TEXT'],
//     handlers: { ... },
//   });
//
// A step is a statement, or an async function of the transaction (to move
// data, say). Steps are never changed or removed once they ran: a change is
// a step of its own.
//
// A shared handler applies the steps its key's database lacks in its own
// session, which stores nothing, then reads: a key never written reads as
// empty tables. A library keeps its tables in a schema of its own name,
// which `schema.ensure(ctx.db)` applies where it uses them, in any object.
import type { Database } from 'restate-sql';
import type { SqlDatabase, SqlSession } from './session.ts';

const TABLE = 'restate_schema';

/** A statement, or an async function of the transaction. */
export type Step = string | ((tx: SqlDatabase) => Promise<void>);

/** A part of a database's schema: its `name`, and the `steps` that make it, in order. */
export class Schema {
  readonly name: string;
  readonly steps: readonly Step[];

  constructor(name: string, steps: readonly Step[]) {
    this.name = name;
    this.steps = steps;
  }

  /**
   * Apply the steps `db` lacks: a library's tables, where it uses them, in
   * any object. On a handler's `ctx.db`, once per attempt, and a shared
   * handler's are its own, vanishing with its session.
   */
  ensure(db: SqlSession | Database): Promise<void> {
    const session = sessions.get(db);
    return session ? session([this]) : apply(db as Database, [this]);
  }
}

/** The handlers' sessions (`ctx.db`), each with how it applies schemas. */
export const sessions = new WeakMap<object, (schemas: readonly Schema[]) => Promise<void>>();

// The schema of each object, by its name.
const objects = new Map<string, Schema>();

/** The object `name`'s own schema: its steps, which each of its databases is made by. */
export const register = (name: string, steps: readonly Step[]) => void objects.set(name, new Schema(name, steps));

/** The object `name`'s own schema, if it has one. */
export const ofObject = (name: string) => objects.get(name);

/** Apply the steps of `schemas` that `db` lacks, in one transaction, with how far each got. */
export async function apply(db: Database, schemas: readonly Schema[]) {
  const done = await applied(db);
  const behind = schemas.filter((s) => (done.get(s.name) ?? 0) < s.steps.length);
  if (behind.length === 0) return;
  await db.transaction(async (tx) => {
    await tx.execute(`CREATE TABLE IF NOT EXISTS ${TABLE} (name TEXT PRIMARY KEY, version INTEGER NOT NULL)`);
    for (const schema of behind) {
      for (const step of schema.steps.slice(done.get(schema.name) ?? 0)) {
        await (typeof step === 'string' ? tx.execute(step) : step(tx));
      }
      await tx.execute(`INSERT OR REPLACE INTO ${TABLE} VALUES (?, ?)`, [schema.name, schema.steps.length]);
    }
  });
}

/** How many steps of each schema `db` has. */
async function applied(db: Database) {
  const exists = await db.query("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", [TABLE]);
  if (exists.length === 0) return new Map<string, number>();
  const rows = await db.query(`SELECT name, version FROM ${TABLE}`);
  return new Map(rows.map(([name, version]) => [String(name), Number(version)]));
}

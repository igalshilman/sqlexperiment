// The schema of an object's databases: each key's database is made by the
// same steps, in order, applied once per key, where its first handler opens
// it. A database keeps how far each schema got in its record `schema/<name>`
// (beside its tables, in none of them), committed with the steps, so a step
// added later (a column, an index) runs on each key's database once, as that
// key is next opened:
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
// empty tables. This repository's libraries (restate-agents) keep their tables
// in a schema of their own name, which `schema.ensure(ctx.db)` applies where
// they use them, in any object: internal, from restate-objects/internal.
import type { Connection, DatabaseWriter, ConnectionTransaction } from './database.ts';
import type { Database } from './session.ts';

/** The database's record of how many steps of the schema `name` it has. */
const recordOf = (name: string) => `schema/${name}`;

/** A statement, or an async function of the transaction. */
export type Step = string | ((tx: DatabaseWriter) => Promise<void>);

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
  ensure(db: Database | Connection): Promise<void> {
    const session = sessions.get(db);
    return session ? session([this]) : apply(db as Connection, [this]);
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
export async function apply(db: Connection, schemas: readonly Schema[]) {
  if ((await behind(db, schemas)).length === 0) return;
  await db.transaction(async (tx) => {
    // Again, now that the transaction holds the session: a call before it
    // may have applied them.
    for (const { schema, done } of await behind(tx, schemas)) {
      for (const step of schema.steps.slice(done)) {
        await (typeof step === 'string' ? tx.execute(step) : step(tx));
      }
      tx.setRecord(recordOf(schema.name), new TextEncoder().encode(String(schema.steps.length)));
    }
  });
}

/** The schemas `db` lacks steps of, with how many it has. */
async function behind(db: Connection | ConnectionTransaction, schemas: readonly Schema[]) {
  const applied = await Promise.all(
    schemas.map(async (schema) => {
      const record = await db.record(recordOf(schema.name));
      return { schema, done: record.length === 0 ? 0 : Number(new TextDecoder().decode(record)) };
    }),
  );
  return applied.filter(({ schema, done }) => done < schema.steps.length);
}

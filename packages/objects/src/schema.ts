// The schema of an object's databases: each key's database is made by the
// same steps, in order, applied once per key, where its first handler opens
// it. A database keeps how far each schema got in its record `schema` (beside
// its tables, in none of them), committed with the steps, so a step
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
// they use them, in any object: internal, from restate-session-objects/internal.
import type { Connection, DatabaseReader, DatabaseWriter, ConnectionTransaction } from './database.ts';

/**
 * The database's record of its schemas: how many steps of each it has, by
 * name. One record, so that `reset` clears all of them, a library's this
 * process never loaded too.
 */
const RECORD = 'schema';

/** How many steps of each schema the database has, by name. */
type Applied = Record<string, number>;

async function appliedIn(db: Connection | ConnectionTransaction): Promise<Applied> {
  const record = await db.record(RECORD);
  return record.length === 0 ? {} : (JSON.parse(new TextDecoder().decode(record)) as Applied);
}

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
  ensure(db: DatabaseReader | Connection): Promise<void> {
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
  if (behind(await appliedIn(db), schemas).length === 0) return;
  // Again in the transaction, which holds the session: a call before it may
  // have applied them.
  await db.transaction((tx) => applyIn(tx, schemas));
}

/** Apply the steps of `schemas` that the transaction `tx` lacks, and record how far each got. */
export async function applyIn(tx: ConnectionTransaction, schemas: readonly Schema[]) {
  const applied = await appliedIn(tx);
  const lacking = behind(applied, schemas);
  if (lacking.length === 0) return;
  for (const schema of lacking) {
    for (const step of schema.steps.slice(applied[schema.name] ?? 0)) {
      await (typeof step === 'string' ? tx.execute(step) : step(tx));
    }
    applied[schema.name] = schema.steps.length;
  }
  tx.setRecord(RECORD, new TextEncoder().encode(JSON.stringify(applied)));
}

/** The schemas of `schemas` that `applied` lacks steps of. */
const behind = (applied: Applied, schemas: readonly Schema[]) =>
  schemas.filter((schema) => (applied[schema.name] ?? 0) < schema.steps.length);

/**
 * Empty the database in the transaction `tx`: drop every table, view, index
 * and trigger, and forget every schema, then apply `schemas` again. Its
 * other records (the outbox) stay. It commits with `tx`: what `tx` writes
 * after it are the new database's first rows.
 */
export async function dropAll(tx: ConnectionTransaction, schemas: readonly Schema[]) {
  const made = await tx.query<{ type: string; name: string }>(
    "SELECT type, name FROM sqlite_schema WHERE substr(name, 1, 7) != 'sqlite_'",
  );
  // A trigger or view may name a table; an index goes with its table.
  for (const type of ['trigger', 'view', 'table']) {
    for (const { name } of made.filter((m) => m.type === type)) {
      await tx.execute(`DROP ${type.toUpperCase()} IF EXISTS "${name.replaceAll('"', '""')}"`);
    }
  }
  tx.setRecord(RECORD, new Uint8Array());
  await applyIn(tx, schemas);
}

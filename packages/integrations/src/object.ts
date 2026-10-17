// Virtual objects set up for SQL and agents.
import * as restate from '@restatedev/restate-sdk';

/**
 * The options of every object here: SQL (`ctx.db`, see restate.sqlOptions),
 * and retries without end, since each attempt resumes from the database.
 */
const OPTIONS = {
  ...restate.sqlOptions,
  retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
} as const;

/**
 * A Restate virtual object with a SQL database of its own, `ctx.db`: one per
 * key, stored with the object's state.
 *
 *   const notes = object({
 *     name: 'notes',
 *     handlers: {
 *       add: (ctx, note: string) => ctx.db.execute('INSERT INTO notes VALUES (?)', [note]),
 *     },
 *   });
 */
/** A handler: `ctx` is the object's context, with `ctx.db`; the input, if any, is annotated. */
type Handler = (ctx: restate.SqlObjectContext, input: never) => unknown;

export const object = <P extends string, H extends Record<string, Handler>>(definition: {
  name: P;
  handlers: H;
  description?: string;
  metadata?: Record<string, string>;
}): restate.VirtualObjectDefinition<P, H> =>
  restate.object<P, H, typeof OPTIONS>({
    ...definition,
    handlers: definition.handlers as restate.ObjectOpts<H, restate.SqlObjectContext, restate.SqlObjectSharedContext>,
    options: OPTIONS,
  });

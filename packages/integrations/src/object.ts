// Virtual objects set up for SQL and agents.
import * as restate from '@restatedev/restate-sdk';

/**
 * The options of every object here: SQL (`ctx.db`: no journal replay, lazy
 * state, no journal kept, see restate.sqlOptions), and retries without end,
 * since each attempt resumes from the database.
 */
const OPTIONS = {
  ...restate.sqlOptions,
  retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
} as const;

/**
 * The metadata of every object here: Restate's UI hides the SQL storage from
 * the journal view (the database's pages, log and header in the object's
 * state, `v13/...`; the sends pending with a transaction; and the `sync` run
 * before an attempt ends).
 */
const METADATA = {
  'dev.restate.tooling.journal.hidden.names': 'sync,_restate_sql_sends',
  'dev.restate.tooling.journal.hidden.name.prefix': 'v13/',
};

/** A handler: `ctx` is the object's context, with `ctx.db`; the input, if any, is annotated. */
type Handler = (ctx: restate.SqlObjectContext, input: never) => unknown;

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
 *
 * `options` and `metadata` add to (or replace) the ones set here, such as
 * `options: { journalRetention: { hours: 1 } }` to look at journals in the UI.
 */
export const object = <P extends string, H extends Record<string, Handler>>(definition: {
  name: P;
  handlers: H;
  description?: string;
  metadata?: Record<string, string>;
  options?: Omit<restate.ObjectOptions, 'journalReplay'>;
}): restate.VirtualObjectDefinition<P, H> =>
  restate.object<P, H, typeof OPTIONS>({
    ...definition,
    handlers: definition.handlers as restate.ObjectOpts<H, restate.SqlObjectContext, restate.SqlObjectSharedContext>,
    metadata: { ...METADATA, ...definition.metadata },
    options: { ...OPTIONS, ...definition.options } as typeof OPTIONS,
  });

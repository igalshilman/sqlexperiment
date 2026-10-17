// Virtual objects set up for SQL and agents.
import * as restate from '@restatedev/restate-sdk';
import { signals, signalsOf } from './signals.ts';

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

/**
 * The handlers every object here has, beside its own: `signal`, which
 * resolves a signal the object's tools wait for (see `signal` in tools.ts),
 * and `pending`, the signals they wait for. Both are shared, so they run
 * while the object's handler waits, and journaled: they use no `ctx.db`.
 */
const SIGNALS = {
  signal: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.ObjectSharedContext, signal: { name: string; value?: unknown }) => {
      await ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).resolve(signal);
    },
  ),
  pending: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.ObjectSharedContext): Promise<string[]> =>
      ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).pending(),
  ),
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
 * Besides its own handlers, it has `signal({ name, value })`, which
 * resolves a signal its tools wait for, and `pending`, the ones they wait
 * for.
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
}): restate.VirtualObjectDefinition<P, typeof SIGNALS & H> =>
  restate.object<P, typeof SIGNALS & H, typeof OPTIONS>({
    ...definition,
    handlers: { ...SIGNALS, ...definition.handlers } as restate.ObjectOpts<
      typeof SIGNALS & H,
      restate.SqlObjectContext,
      restate.SqlObjectSharedContext
    >,
    metadata: { ...METADATA, ...definition.metadata },
    options: { ...OPTIONS, ...definition.options } as typeof OPTIONS,
  });

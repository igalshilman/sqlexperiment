// Virtual objects set up for SQL and agents.
import * as restate from '@restatedev/restate-sdk';
import { type Resolved, type Signal, signals, signalsOf, type Waiting } from './signals.ts';

/**
 * The options of every object here: SQL (`ctx.db`: no journal replay, lazy
 * state, no journal kept, see restate.sqlOptions), and retries for weeks,
 * since each attempt resumes from the database: soon after a crash, then
 * twice as long each time, up to 10 seconds apart (a provider's rate limit,
 * say, is not asked again twice a second).
 */
const OPTIONS = {
  ...restate.sqlOptions,
  retryPolicy: { initialInterval: 50, exponentiationFactor: 2, maxInterval: 10_000, maxAttempts: 100_000 },
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
 * resolves a wait of the object's tools (see `signal` in tools.ts), and
 * `pending`, the waits. Both are shared, so they run while the object's
 * handler waits, and journaled: they use no `ctx.db`.
 */
const SIGNALS = {
  signal: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.ObjectSharedContext, signal: Signal): Promise<Resolved> =>
      ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).resolve(signal),
  ),
  pending: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.ObjectSharedContext): Promise<Waiting[]> =>
      ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).pending(),
  ),
};

/** A handler: `ctx` is the object's context, with `ctx.db`; the input, if any, is annotated. */
type Handler = (ctx: restate.SqlObjectContext, input: never) => unknown;

/** The names of the handlers every object has, which its own handlers cannot take. */
type Reserved = { [name in keyof typeof SIGNALS]?: 'a handler every object has: name yours otherwise' };

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
 * Besides its own handlers, it has `pending`, the waits of its tools
 * (`signal()`), and `signal({ id, value })`, which resolves one.
 *
 * `options` and `metadata` add to (or replace) the ones set here, such as
 * `options: { journalRetention: { hours: 1 } }` to look at journals in the
 * UI, or `options: { retryPolicy: ... }`.
 */
export const object = <P extends string, H extends Record<string, Handler>>(definition: {
  name: P;
  handlers: H & Reserved;
  description?: string;
  metadata?: Record<string, string>;
  options?: Omit<restate.ObjectOptions, 'journalReplay'>;
}): restate.VirtualObjectDefinition<P, typeof SIGNALS & H> => {
  for (const name of Object.keys(SIGNALS)) {
    if (name in definition.handlers)
      throw new Error(`${definition.name}: every object has a \`${name}\` handler already`);
  }
  return restate.object<P, typeof SIGNALS & H, typeof OPTIONS>({
    ...definition,
    handlers: { ...SIGNALS, ...definition.handlers } as restate.ObjectOpts<
      typeof SIGNALS & H,
      restate.SqlObjectContext,
      restate.SqlObjectSharedContext
    >,
    metadata: { ...METADATA, ...definition.metadata },
    options: { ...OPTIONS, ...definition.options } as typeof OPTIONS,
  });
};

// Virtual objects set up for SQL and agents.
import * as restate from '@restatedev/restate-sdk';
import * as sql from 'restate-sql';
import { messageSerde, TURN, type Turn } from './common.ts';
import {
  type Notifications,
  type Resolved,
  type Signal,
  type Steered,
  signals,
  signalsOf,
  type Waiting,
  type Watch,
} from './signals.ts';

/**
 * The options of every object here, beside SQL's (`sql.options`): retries for
 * weeks, since each attempt resumes from the database: soon after a crash,
 * then twice as long each time, up to 10 seconds apart (a provider's rate
 * limit, say, is not asked again twice a second).
 */
const OPTIONS = {
  retryPolicy: { initialInterval: 50, exponentiationFactor: 2, maxInterval: 10_000, maxAttempts: 100_000 },
} satisfies restate.ObjectOptions;

/** What an object is doing: the message it answers, if any, and what its tools wait for. */
export interface Status {
  /** The message being answered (by a `chat` handler), or null. */
  answering: Turn | null;
  /** The waits of its tools, oldest first (see `pending`). */
  waiting: Waiting[];
}

/** What `stop` did: the invocation of the message it stopped, or null if none was answered. */
export interface Stopped {
  stopped: string | null;
}

/**
 * The handlers every object here has, beside its own: `signal`, which
 * resolves a wait of the object's tools (see `signal` in tools.ts),
 * `pending`, the waits, `status`, what the object is doing, `stop`, which
 * stops the message it answers, `steer`, which adds to it, and
 * `notifications` and `watch`, the revision of its conversation, and a wait
 * for a newer one. They are shared, so they run while the object's handler
 * works, and replay their journal: they use no `ctx.db`.
 */
const BUILT_IN = {
  signal: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext, signal: Signal): Promise<Resolved> =>
    ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).resolve(signal),
  ),
  pending: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext): Promise<Waiting[]> =>
    ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).pending(),
  ),
  status: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext): Promise<Status> => ({
    answering: await ctx.get<Turn>(TURN),
    waiting: await ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).pending(),
  })),
  /** Cancel the invocation of the message being answered (`status`'s), as Restate's admin API cancels one. */
  stop: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
    const turn = await ctx.get<Turn>(TURN);
    if (turn) ctx.invocation(restate.InvocationIdParser.fromString(turn.invocationId)).cancel();
    return { stopped: turn?.invocationId ?? null } satisfies Stopped;
  }),
  /**
   * Add `text` to the message being answered (`status`'s), such as "Only
   * Monday, please": the agent takes it at its next tool call, as its
   * framework steers a turn. A turn that ends first answers it as a message
   * of its own, as `chat` does when no message is answered.
   */
  steer: restate.handlers.object.shared(
    { input: messageSerde, accept: '*/*' },
    async (ctx: restate.ObjectSharedContext, text: string): Promise<Steered> => {
      const agent = { service: ctx.request().target.service, key: ctx.key };
      const turn = await ctx.get<Turn>(TURN);
      return ctx
        .objectClient(signals, signalsOf(agent.service, agent.key))
        .steer({ agent, turn: turn?.invocationId ?? null, text });
    },
  ),
  /** The revision of the conversation: each change of what `messages` and `status` give increases it. */
  notifications: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext): Promise<Notifications> =>
    ctx.objectClient(signals, signalsOf(ctx.request().target.service, ctx.key)).notifications(),
  ),
  /**
   * The revision, once it is past `afterRevision`, or at the timeout (50 s by
   * default, at most 60 s), whichever comes first: a client then reads what
   * changed. It waits in Restate, holding no process; a client that retries
   * it with the same idempotency key joins the same wait.
   */
  watch: restate.handlers.object.shared(
    async (ctx: restate.ObjectSharedContext, { afterRevision, timeoutSeconds = 50 }: Watch): Promise<Notifications> => {
      const key = signalsOf(ctx.request().target.service, ctx.key);
      const { id, promise } = ctx.awakeable<Notifications>();
      ctx.objectSendClient(signals, key).subscribe({ afterRevision, awakeable: id });
      const timeout = ctx.sleep({ seconds: Math.min(Math.max(timeoutSeconds, 0), 60) }).map(() => null);
      const changed = await restate.RestatePromise.race([promise, timeout]);
      if (changed) return changed;
      ctx.objectSendClient(signals, key).unsubscribe(id);
      return ctx.objectClient(signals, key).notifications();
    },
  ),
};

/** A handler: `ctx` is the object's context, with `ctx.db`; the input, if any, is annotated. */
type Handler = (ctx: sql.ObjectContext, input: never) => unknown;

/** The names of the handlers every object has, which its own handlers cannot take. */
type Reserved = { [name in keyof typeof BUILT_IN]?: 'a handler every object has: name yours otherwise' };

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
 * Besides its own handlers, it has `status`, what it is doing (the message
 * it answers, and what its tools wait for), `stop`, which stops that
 * message, `steer`, which adds to it, `pending`, the waits of its tools
 * (`signal()`), `signal({ id, value })`, which resolves one, and
 * `notifications` and `watch({ afterRevision })`, to follow its
 * conversation (`follow()` of restate-agents/client).
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
  options?: restate.ObjectOptions;
}): restate.VirtualObjectDefinition<P, typeof BUILT_IN & H> => {
  for (const name of Object.keys(BUILT_IN)) {
    if (name in definition.handlers)
      throw new Error(`${definition.name}: every object has a \`${name}\` handler already`);
  }
  return sql.object({
    ...definition,
    handlers: { ...BUILT_IN, ...definition.handlers },
    options: { ...OPTIONS, ...definition.options },
  });
};

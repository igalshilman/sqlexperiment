// Virtual objects set up for SQL and agents.
import * as restate from 'restate-fns';
import { inBackground, type Messages, type MessagesRequest, messageSerde, refusal, TURN, type Turn } from './common.ts';
import * as log from './log.ts';
import {
  agentOf,
  type Notifications,
  type Resolved,
  type Signal,
  type Steered,
  signals,
  signalsOf,
  withCompanion,
  type Waiting,
  type Watch,
} from './signals.ts';

/**
 * The options of every object here, beside SQL's (`restate.sqlOptions`): retries for
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
 * for a newer one (an agent's has `clear` and `sync` too: `FOLLOWED`).
 * They are shared, so they run while the object's handler
 * works, and replay their journal: they use no `ctx.db`.
 */
const BUILT_IN = {
  signal: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.JournaledObjectSharedContext, signal: Signal): Promise<Resolved> =>
      ctx.objectClient(signals, signalsOf(ctx)).resolve(signal),
  ),
  pending: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.JournaledObjectSharedContext): Promise<Waiting[]> =>
      ctx.objectClient(signals, signalsOf(ctx)).pending(),
  ),
  status: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.JournaledObjectSharedContext): Promise<Status> => ({
      answering: await ctx.get<Turn>(TURN),
      waiting: await ctx.objectClient(signals, signalsOf(ctx)).pending(),
    }),
  ),
  /** Cancel the invocation of the message being answered (`status`'s), as Restate's admin API cancels one. */
  stop: restate.handlers.object.shared({ journalReplay: true }, async (ctx: restate.JournaledObjectSharedContext) => {
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
    { journalReplay: true, input: messageSerde, accept: '*/*' },
    async (ctx: restate.JournaledObjectSharedContext, text: string): Promise<Steered> => {
      const turn = await ctx.get<Turn>(TURN);
      return ctx
        .objectClient(signals, signalsOf(ctx))
        .steer({ agent: agentOf(ctx), turn: turn?.invocationId ?? null, text });
    },
  ),
  /** The revision of the conversation: each change of what `messages` and `status` give increases it. */
  notifications: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.JournaledObjectSharedContext): Promise<Notifications> =>
      ctx.objectClient(signals, signalsOf(ctx)).notifications(),
  ),
  /**
   * The revision, once there is one newer than `revision`, or at the timeout (50 s by
   * default, at most 60 s), whichever comes first: a client then reads what
   * changed. It waits in Restate, holding no process; a client that retries
   * it with the same idempotency key joins the same wait.
   */
  watch: restate.handlers.object.shared(
    { journalReplay: true },
    async (ctx: restate.JournaledObjectSharedContext, watch: Watch): Promise<Notifications> => waited(ctx, watch),
  ),
};

/**
 * The handlers that agents (an object with `messages`) have beside those of
 * every object: `clear`, which forgets the conversation; and `sync`, the conversation since `cursor` (by default, all of
 * it), what the agent is doing, and the revision, in one request; with
 * `revision`, once there is a newer one, or at the timeout. A client follows
 * the conversation by sending each answer's `revision` and `cursor` back, at
 * once while it has `more`.
 */
const FOLLOWED = {
  /**
   * Forget the conversation: stop the message being answered (as `stop`),
   * then clear the key's conversation, database and state, and what its
   * `signals` hold (waits, kept signals, steered messages). The log's ids
   * and cursors go on (see log.ts), so a follower reads on, and drops the
   * messages before `first`. A message sent before it is answered first,
   * then forgotten too; one sent after starts anew.
   */
  clear: restate.handlers.object.shared({ journalReplay: true }, async (ctx: restate.JournaledObjectSharedContext) => {
    const turn = await ctx.get<Turn>(TURN);
    if (turn) ctx.invocation(restate.InvocationIdParser.fromString(turn.invocationId)).cancel();
    await own(ctx, 'erase');
    return { stopped: turn?.invocationId ?? null } satisfies Stopped;
  }),
  sync: restate.handlers.object.shared(
    { journalReplay: true },
    async (
      ctx: restate.JournaledObjectSharedContext,
      { cursor = 0, revision, timeoutSeconds = 50, limit = 100 }: SyncRequest = {},
    ): Promise<Synced> => {
      if (revision !== undefined) await waited(ctx, { revision, timeoutSeconds });
      // The revision first: a change after it shows in the next sync. Then
      // the object's own handlers, which read its state as it is now.
      const now = await ctx.objectClient(signals, signalsOf(ctx)).notifications();
      const status = await own<Status>(ctx, 'status');
      const page = await own<Messages>(ctx, 'messages', { cursor, limit } satisfies MessagesRequest);
      return { revision: now.revision, status, ...page };
    },
  ),
};

/**
 * The agents' handlers that only their own handlers call (Restate's ingress
 * refuses them), and their types leave out: `erase`, `clear`'s work, once
 * the handlers before it are done. It clears the key's state (its database
 * too) and its signals', and the log goes on where it got to (see log.ts).
 */
const INTERNAL = {
  erase: restate.handlers.object.exclusive(
    { journalReplay: true, ingressPrivate: true },
    async (ctx: restate.JournaledObjectContext) => {
      const start = startAfter(await own<Messages>(ctx, 'messages', { limit: 1 }));
      ctx.clearAll();
      ctx.set(log.START, start);
      await ctx.objectClient(signals, signalsOf(ctx)).reset();
    },
  ),
};

/** Where the log starts after `latest`, its latest message: past its ids and cursor. */
const startAfter = ({ messages: [latest], cursor, first = 0 }: Messages): log.Start => ({
  seq: cursor,
  id: latest ? latest.id + 1 : first,
});

/** A call of a handler of the object of `ctx`, the same key. */
const own = <O>(ctx: restate.JournaledObjectSharedContext, method: string, parameter?: unknown) =>
  ctx.genericCall<unknown, O>({
    service: ctx.request().target.service,
    method,
    key: ctx.key,
    parameter,
    inputSerde: restate.serde.json,
    outputSerde: restate.serde.json,
  });

/** The revision, once there is one newer than `revision`, or at the timeout: see `watch`. */
async function waited(
  ctx: restate.JournaledObjectSharedContext,
  { revision, timeoutSeconds = 50 }: Watch,
): Promise<Notifications> {
  const key = signalsOf(ctx);
  const { id, promise } = ctx.awakeable<Notifications>();
  ctx.objectSendClient(signals, key).subscribe({ revision, awakeable: id });
  const timeout = ctx.sleep({ seconds: Math.min(Math.max(timeoutSeconds, 0), 60) }).map(() => null);
  const changed = await restate.RestatePromise.race([promise, timeout]);
  if (changed) return changed;
  ctx.objectSendClient(signals, key).unsubscribe(id);
  return ctx.objectClient(signals, key).notifications();
}

/**
 * What `sync` reads: what is new or changed since `cursor` (default 0, all
 * of it), at most `limit` messages (default 100); once the revision is past
 * `revision`, if given, or at `timeoutSeconds` (default 50, at most 60).
 */
export interface SyncRequest {
  cursor?: number;
  revision?: number;
  timeoutSeconds?: number;
  limit?: number;
}

/** What `sync` answers: the messages since the cursor, as `messages` gives them, the status and the revision. */
export interface Synced extends Messages {
  revision: number;
  status: Status;
}

/** An object's (or agent's) name, its description, and the tables of your own in its database, if any (see `object`). */
export interface Definition<P extends string> {
  name: P;
  /** What it is: for the UI, and for the model of a parent that hands it tasks (`subagent`). */
  description?: string;
  schema?: readonly restate.Step[];
  /**
   * Whether it works unwatched, such as a subagent: its turn calls its
   * `signals` only at its end, not at each step, so a client that follows it
   * sees each turn once it ended, and a steered message waits for the turn's
   * end. `messages` has every turn as before.
   */
  background?: boolean;
}

/** A handler: `ctx` is the object's context, with `ctx.db`; the input, if any, is annotated. */
type Handler = (ctx: restate.ObjectContext, input: never) => unknown;

/** The names of the handlers every object (or agent) has, which its own handlers cannot take. */
type Reserved = {
  [
    name in keyof typeof BUILT_IN | keyof typeof FOLLOWED | keyof typeof INTERNAL
  ]?: 'a handler every object has: name yours otherwise';
};

/** The handlers an object with `handlers` has: those of every object, an agent's `clear` and `sync`, and its own. */
type With<H> = typeof BUILT_IN & (H extends { messages: unknown } ? typeof FOLLOWED : unknown) & H;

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
 * `notifications` and `watch({ revision })`, to follow its conversation.
 * An agent, an object with a `messages` handler (such as `claude.messages()`),
 * has `clear` too, which forgets its conversation (its database and state
 * too), and `sync`, which `follow()` of restate-agents/client reads.
 *
 * `schema`, optional: tables of your own, beside the agent's (which are its
 * own business), made in each key's database as restate-fns's `object` makes
 * them. Any name is yours but those starting with `restate_`, the agent's
 * (`restate_pi_tasks`, `restate_claude_messages`, `restate_agent_log`...);
 * OpenCode's tables are in a database of their own.
 *
 * `options` and `metadata` add to (or replace) the ones set here, such as
 * `options: { journalRetention: { hours: 1 } }` to look at journals in the
 * UI, or `options: { retryPolicy: ... }`.
 */
export const object = <P extends string, H extends Record<string, Handler>>(
  definition: Definition<P> & {
    handlers: H & Reserved;
    metadata?: Record<string, string>;
    options?: restate.ObjectOptions;
  },
): restate.VirtualObjectDefinition<P, With<H>> => {
  const { background, ...rest } = definition;
  if (background) inBackground(definition.name);
  for (const name of [...Object.keys(BUILT_IN), ...Object.keys(FOLLOWED), ...Object.keys(INTERNAL)]) {
    if (name in definition.handlers)
      throw new Error(`${definition.name}: \`${name}\` is a handler of restate-agents' own: name yours otherwise`);
  }
  const yours = Object.entries(definition.handlers as Record<string, Handler>).map(([name, handler]) => [
    name,
    // A plain function: a model provider's refusal (a missing API key) ends its invocation.
    Object.getOwnPropertySymbols(handler).length === 0 ? answering(handler) : handler,
  ]);
  const handlers: object = {
    ...BUILT_IN,
    ...('messages' in definition.handlers ? { ...FOLLOWED, ...INTERNAL } : {}),
    ...Object.fromEntries(yours),
  };
  return withCompanion(
    restate.object({
      ...rest,
      handlers: handlers as With<H>,
      options: { ...OPTIONS, ...definition.options },
    }),
  );
};

/** `handler`, but a model provider's refusal it fails with ends its invocation, as `chat`'s does. */
const answering =
  (handler: Handler): Handler =>
  async (ctx, input) => {
    try {
      return await handler(ctx, input);
    } catch (error) {
      throw refusal(error) ?? error;
    }
  };

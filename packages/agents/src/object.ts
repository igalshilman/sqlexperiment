// Virtual objects set up for SQL and agents.
import * as restate from 'restate-sessions';
import { reset } from 'restate-sessions/internal';
import {
  cleared,
  onClear,
  inBackground,
  type Messages,
  type MessagesRequest,
  messageSerde,
  refusal,
  type Turn,
} from './common.ts';
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
 * The options of every object here, beside a session object's: retries for
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
 * A call of the companion's of `ctx`; with `step`, a change, keyed by the
 * request and `step`: a restarted view joins the same call. A read has no
 * key: asked again, it answers again, and Restate keeps nothing of it.
 */
const companion = (ctx: restate.ViewContext, step?: string) =>
  ctx.rpc(signals, signalsOf(ctx), step === undefined ? {} : { idempotencyKey: `${ctx.request.id}:${step}` });

/**
 * Views that wait on the companion (`watch`, `sync`, up to a minute) or on
 * the object (`clear`): Restate
 * suspends one that waits past its inactivity timeout, which restarts it,
 * so it is longer than the wait.
 */
const WAITS = { inactivityTimeout: { minutes: 5 } } satisfies restate.HandlerOptions;

/**
 * The views every object here has, beside its own: `signal`, which resolves
 * a wait of the object's tools (see `signal` in tools.ts), `pending`, the
 * waits, `status`, what the object is doing, `stop`, which stops the message
 * it answers, `steer`, which adds to it, and `notifications` and `watch`,
 * the revision of its conversation, and a wait for a newer one (an agent's
 * has `clear` and `sync` too: {@link followed}). Each asks the object's
 * `signals` companion, which knows what the object does: they run while the
 * object's handler works.
 */
const BUILT_IN = {
  signal: async (ctx: restate.ViewContext, signal: Signal): Promise<Resolved> =>
    companion(ctx, 'signal').resolve(signal),
  pending: async (ctx: restate.ViewContext): Promise<Waiting[]> => companion(ctx).pending(),
  status: async (ctx: restate.ViewContext): Promise<Status> => companion(ctx).status(),
  /** Cancel the invocation of the message being answered (`status`'s), as Restate's admin API cancels one. */
  stop: async (ctx: restate.ViewContext): Promise<Stopped> => companion(ctx, 'stop').stop(),
  /**
   * Add `text` to the message being answered (`status`'s), such as "Only
   * Monday, please": the agent takes it at its next tool call, as its
   * framework steers a turn. A turn that ends first answers it as a message
   * of its own, as `chat` does when no message is answered.
   */
  steer: {
    input: messageSerde,
    accept: '*/*',
    handler: async (ctx: restate.ViewContext, text: string): Promise<Steered> =>
      companion(ctx, 'steer').steer({ agent: agentOf(ctx), text }),
  },
  /** The revision of the conversation: each change of what `messages` and `status` give increases it. */
  notifications: async (ctx: restate.ViewContext): Promise<Notifications> => companion(ctx).notifications(),
  /**
   * The revision, once there is one newer than `revision`, or at the timeout (50 s by
   * default, at most 60 s), whichever comes first: a client then reads what
   * changed. It waits in Restate, holding no process; a client that retries
   * it with the same idempotency key joins the same wait.
   */
  watch: {
    ...WAITS,
    handler: async (ctx: restate.ViewContext, watch: Watch): Promise<Notifications> =>
      companion(ctx, 'watch').watch(watch),
  },
};

/**
 * The views that agents (an object with a `messages` view) have beside
 * those of every object: `clear`, which forgets the conversation; and
 * `sync`, the conversation since `cursor` (by default, all of it), what the
 * agent is doing, and the revision, in one request; with `revision`, once
 * there is a newer one, or at the timeout. A client follows the
 * conversation by sending each answer's `revision` and `cursor` back, at
 * once while it has `more`.
 */
const followed = (messages: (ctx: restate.ViewContext, request: MessagesRequest) => Promise<Messages>) => ({
  /**
   * Forget the conversation: stop the message being answered (as `stop`),
   * then empty the key's database, and what its `signals` hold (waits, kept
   * signals, steered messages). The log's ids and cursors go on (see
   * log.ts), so a follower reads on, and drops the messages before `first`.
   * A message sent before it is answered first, then forgotten too; one sent
   * after starts anew.
   */
  clear: {
    // `erase` waits for the messages sent before it.
    ...WAITS,
    handler: async (ctx: restate.ViewContext): Promise<Stopped> => {
      const stopped = await companion(ctx, 'clear').stop();
      await ctx.rpc({
        service: ctx.request.target.service,
        key: ctx.key,
        handler: 'erase',
        idempotencyKey: `${ctx.request.id}:erase`,
      });
      return stopped;
    },
  },
  sync: {
    ...WAITS,
    handler: async (
      ctx: restate.ViewContext,
      { cursor = 0, revision, timeoutSeconds = 50, limit = 100 }: SyncRequest = {},
    ): Promise<Synced> => {
      if (revision !== undefined) await companion(ctx, 'watch').watch({ revision, timeoutSeconds });
      // The revision first: a change after it shows in the next sync. Then
      // what the object does, and its conversation, as they are now.
      const now = await companion(ctx).notifications();
      const status = await companion(ctx).status();
      return { revision: now.revision, status, ...(await messages(ctx, { cursor, limit })) };
    },
  },
});

/**
 * The agents' handlers that only their own views call (Restate's ingress
 * refuses them), and their types leave out: `erase`, `clear`'s work, once
 * the handlers before it are done. It empties the key's database and its
 * signals', and the log goes on where it got to (see log.ts).
 */
const INTERNAL = {
  erase: {
    ingressPrivate: true,
    handler: async (ctx: restate.SessionContext) => {
      const start = startAfter(await log.page(ctx.db, { limit: 1 }));
      await ctx.db.transaction(async (tx) => {
        await reset(tx);
        await log.restart(tx, start);
      });
      await cleared(ctx);
      await ctx.rpc(signals, signalsOf(ctx), { idempotencyKey: `${ctx.request.id}:reset` }).reset();
    },
  },
};

/** Where the log starts after `latest`, its latest message: past its ids and cursor. */
const startAfter = ({ messages: [latest], cursor, first = 0 }: Messages): log.Start => ({
  seq: cursor,
  id: latest ? latest.id + 1 : first,
});

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

/** An object's (or agent's) name, its description, and the tables of your own in its database, if any (see `sessionObject`). */
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

/** Each entry's function, as clients see it; none for entries left out. */
type Functions<H> = string extends keyof H ? unknown : { [K in keyof H]: H[K] extends { handler: infer F } ? F : H[K] };

/** The names of the handlers and views every object (or agent) has, which its own cannot take. */
type Reserved = {
  [
    name in keyof typeof BUILT_IN | keyof ReturnType<typeof followed> | keyof typeof INTERNAL
  ]?: 'a handler every object has: name yours otherwise';
};

/** What an object with `handlers` and `views` has: those of every object, an agent's `clear` and `sync`, and its own. */
type With<H, V> = Functions<typeof BUILT_IN> &
  (V extends { messages: unknown } ? Functions<ReturnType<typeof followed>> : unknown) &
  Functions<H> &
  Functions<V>;

/**
 * A Restate virtual object with a SQL database per key, `ctx.db`: a
 * `restate.sessionObject` of restate-sessions, set up for agents.
 *
 *   const notes = sessionObject({
 *     name: 'notes',
 *     handlers: {
 *       add: (ctx, note: string) => ctx.db.execute('INSERT INTO notes VALUES (?)', [note]),
 *     },
 *   });
 *
 * Besides its own handlers and views, it has the views `status`, what it is
 * doing (the message it answers, and what its tools wait for), `stop`,
 * which stops that message, `steer`, which adds to it, `pending`, the waits
 * of its tools (`signal()`), `signal({ id, value })`, which resolves one,
 * and `notifications` and `watch({ revision })`, to follow its
 * conversation. An agent, an object with a `messages` view (such as
 * `claude.messages()`), has `clear` too, which forgets its conversation
 * (its database too), and `sync`, which `follow()` of restate-agents/client
 * reads.
 *
 * `schema`, optional: tables of your own, beside the agent's (which are its
 * own business), made in each key's database as a session object makes
 * them. Any name is yours but those starting with `restate_`, the agent's
 * (`restate_pi_tasks`, `restate_claude_messages`, `restate_agent_log`...);
 * OpenCode's tables are in a database of their own.
 *
 * `options` and `metadata` add to (or replace) the ones set here, such as
 * `options: { retryPolicy: ... }`.
 */
export const sessionObject = <
  P extends string,
  H extends Record<string, restate.Entry<restate.SessionContext>>,
  V extends Record<string, restate.Entry<restate.ViewContext>>,
>(
  definition: Definition<P> & {
    handlers?: H & Reserved;
    views?: V & Reserved;
    metadata?: Record<string, string>;
    options?: restate.ObjectOptions;
  },
): restate.VirtualObjectDefinition<P, With<H, V>> => {
  const { name, description, schema, background, handlers = {}, views = {}, metadata, options } = definition;
  if (background) inBackground(name);
  const own = [
    ...Object.keys(BUILT_IN),
    ...Object.keys(followed(async () => ({}) as Messages)),
    ...Object.keys(INTERNAL),
  ];
  if ('messages' in handlers) {
    throw new Error(`${name}: \`messages\` is a view: put it in \`views\`, so that the object is an agent`);
  }
  for (const key of own) {
    if (key in handlers || key in views)
      throw new Error(`${name}: \`${key}\` is a handler of restate-agents' own: name yours otherwise`);
  }
  // A model provider's refusal (a missing API key) ends a handler's invocation.
  const yours = Object.fromEntries(
    Object.entries(handlers as Record<string, restate.Entry<restate.SessionContext>>).map(([key, entry]) => [
      key,
      answering(entry),
    ]),
  );
  const messages = (views as Record<string, restate.Entry<restate.ViewContext>>).messages;
  onClear(name, messages);
  const page = (typeof messages === 'function' ? messages : messages?.handler) as
    | ((ctx: restate.ViewContext, request: MessagesRequest) => Promise<Messages>)
    | undefined;
  const agent = page === undefined ? {} : followed(page);
  return withCompanion(
    restate.sessionObject({
      name,
      description,
      schema,
      handlers: { ...(messages === undefined ? {} : INTERNAL), ...yours },
      views: { ...BUILT_IN, ...agent, ...views },
      metadata,
      options: { ...OPTIONS, ...options },
    }),
  );
};

/** `entry`, but a model provider's refusal it fails with ends its invocation, as `chat`'s does. */
function answering(entry: restate.Entry<restate.SessionContext>): restate.Entry<restate.SessionContext> {
  const { handler, ...options } = typeof entry === 'function' ? { handler: entry } : entry;
  return {
    ...options,
    handler: async (ctx: restate.SessionContext, input: unknown) => {
      try {
        return await handler(ctx, input);
      } catch (error) {
        throw refusal(error) ?? error;
      }
    },
  };
}

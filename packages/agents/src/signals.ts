// Signals to agents: a virtual object per agent, keyed `<agent>:<key>` (such
// as `assistant:alice`), whose `wait` returns once `resolve` gives the wait
// its value; the message the agent answers; and the revision of the agent's
// conversation, which its `watch` waits on.
//
// An agent's own handler cannot wait on an awakeable (it has no journal to
// replay one from), so it calls `wait`, which can: a journaled handler of
// this object, which waits without holding a process.
import type { SessionContext } from 'restate-sessions';
import { sdk as restate } from 'restate-sessions/internal';
import type { Turn } from './common.ts';
import type { Status } from './object.ts';

/** An agent's context, as far as its signals need it: any handler's or view's of its object. */
type AgentContext = { readonly key: string; readonly request: { readonly target: { readonly service: string } } };

/** The key of the signals of agent `service`'s key `key`: `<agent>:<key>`. */
const keyOf = (service: string, key: string) => `${service}:${key}`;

/** The key of the signals of the agent of `ctx`. */
export const signalsOf = (ctx: AgentContext) => keyOf(ctx.request.target.service, ctx.key);

/** Where an object of restate-agents (`sessionObject()`, every `agent()`) points to its companion. */
const COMPANION = Symbol('restate-agents.companion');

/**
 * An agent's companion, for one of its keys: its `signals` object, which
 * knows what the agent is doing (the message it answers, what its tools wait
 * for), and how to reach it.
 */
export interface Companion {
  /** The `signals` object, as a client takes it. */
  object: typeof signals;
  /** The key of the agent's key there: `<agent>:<key>`. */
  key: string;
  /** The agent, and its key. */
  agent: Agent;
}

/** `definition`, pointing to its companion: what `sessionObject()` returns. */
export const withCompanion = <D extends { name: string }>(definition: D): D =>
  Object.defineProperty(definition, COMPANION, {
    value: (key: string): Companion => ({
      object: signals,
      key: keyOf(definition.name, key),
      agent: { service: definition.name, key },
    }),
  });

/**
 * The companion of `agent`'s key `key`: for anyone holding an agent (an
 * `sessionObject()` or `agent()` of restate-agents) to reach what it is doing.
 */
export function companionOf(agent: { name: string }, key: string): Companion {
  const companion = (agent as { [COMPANION]?: (key: string) => Companion })[COMPANION];
  if (!companion) throw new Error(`${agent.name} is not an agent of restate-agents (a sessionObject() or an agent())`);
  return companion(key);
}

/** Whether `agent` is an object of restate-agents, with a companion. */
export const hasCompanion = (agent: object) => COMPANION in agent;

/**
 * Deliver `text` to `agent`'s key `key`, from any handler: a message of its
 * own (as `chat`), which it answers after the messages before it. It is
 * sent, not awaited: `opts` may give it an idempotency key, so that it is
 * delivered once.
 *
 *   deliver(ctx, triage, 'ENG-1', 'The issue was closed', { idempotencyKey: delivery });
 */
export function deliver(
  ctx: Pick<restate.Context, 'genericSend'> | Pick<SessionContext, 'send'>,
  agent: { name: string },
  key: string,
  text: string,
  { idempotencyKey }: { idempotencyKey?: string } = {},
) {
  companionOf(agent, key); // an agent of restate-agents, with a `chat` handler
  const message = { service: agent.name, handler: 'chat', key, input: text, idempotencyKey };
  // A session object's handler, or one of the SDK's.
  if ('send' in ctx) ctx.send(message);
  else {
    ctx.genericSend({
      service: agent.name,
      method: 'chat',
      key,
      parameter: text,
      inputSerde: restate.serde.json,
      idempotencyKey,
    });
  }
}

/** A wait for a signal, as `pending` lists it. */
export interface Waiting {
  /** The wait's id, which `signal` takes to resolve this wait and no other. */
  id: string;
  /** The signal it waits for, such as `approval`. */
  name: string;
  /** What it waits for, as the model said it (such as the refund to approve), if the tool takes an input. */
  input?: unknown;
  /** When it started, as an ISO date. */
  since: string;
}

/** Which wait a signal resolves, and with what. */
export interface Signal {
  /** The wait of this id (from `pending`). */
  id?: string;
  /** Else the oldest wait for the signal of this name. */
  name?: string;
  /** Its value: what the wait answers the model with. */
  value?: unknown;
  /** With a `name` and no one waiting for it, keep the signal for the next wait for it (by default it is dropped). */
  keep?: boolean;
}

/** What `signal` did: the id of the wait it resolved, or null if none waited (and it was dropped, or kept). */
export interface Resolved {
  resolved: string | null;
}

/** The revision of an agent's conversation: each change of what a client reads increases it. */
export interface Notifications {
  revision: number;
}

/** What `watch` waits for: a revision newer than `revision`, for at most `timeoutSeconds` (default 50, at most 60). */
export interface Watch {
  revision: number;
  timeoutSeconds?: number;
}

/** An agent: its object's name, and key. */
interface Agent {
  service: string;
  key: string;
}

/**
 * The message the agent answers; the waits, in the order they started, each
 * with its awakeable; the signals kept for the next wait; and the
 * conversation's revision, with the `watch`es waiting for a newer one.
 */
interface State {
  /** The message the agent answers, from its turn's start to its close. */
  turn: Turn;
  waiting: (Waiting & { awakeable: string })[];
  kept: Record<string, unknown[]>;
  revision: number;
  watching: { revision: number; awakeable: string }[];
}

/** Increase the revision, and answer each `watch` waiting for it: the new revision. */
async function publish(ctx: restate.ObjectContext<State>): Promise<Notifications> {
  const revision = ((await ctx.get('revision')) ?? 0) + 1;
  ctx.set('revision', revision);
  for (const { awakeable } of (await ctx.get('watching')) ?? []) ctx.resolveAwakeable(awakeable, { revision });
  ctx.clear('watching');
  return { revision };
}

/** A handler that the agents' own handlers call, which clients cannot (through Restate's ingress). */
const internal = <I, O>(fn: (ctx: restate.ObjectContext<State>, input: I) => Promise<O>) =>
  restate.handlers.object.exclusive({ ingressPrivate: true }, fn);

/**
 * The signals of every agent, keyed `<agent>:<key>` ({@link signalsOf}).
 * `serve` serves it beside the agents; the handlers marked `@internal` are
 * called by the agents' own handlers, and Restate's ingress refuses them.
 */
export const signals = restate.object({
  name: 'signals',
  handlers: {
    /** Wait for the signal `name`, as wait `id` (for `input`), and return its value. */
    wait: restate.handlers.object.shared(
      async (ctx: restate.ObjectSharedContext<State>, wait: { id: string; name: string; input?: unknown }) => {
        const { id, promise } = ctx.awakeable<unknown>();
        const since = new Date(await ctx.date.now()).toISOString();
        ctx.objectSendClient(signals, ctx.key).listen({ ...wait, since, awakeable: id });
        try {
          return await promise;
        } catch (error) {
          // The agent's invocation was cancelled: no one waits any more.
          if (error instanceof restate.CancelledError) ctx.objectSendClient(signals, ctx.key).forget(wait.id);
          throw error;
        }
      },
    ),

    /** Resolve the wait `id`, or else the oldest wait for `name`, with `value`. */
    resolve: async (ctx: restate.ObjectContext<State>, { id, name, value = null, keep = false }: Signal) => {
      if (id === undefined && name === undefined) throw new restate.TerminalError('a signal needs an id or a name');
      const waiting = (await ctx.get('waiting')) ?? [];
      const at = waiting.findIndex((w) => (id === undefined ? w.name === name : w.id === id));
      if (at < 0) {
        if (keep && name !== undefined && id === undefined) {
          const kept = (await ctx.get('kept')) ?? {};
          ctx.set('kept', { ...kept, [name]: [...(kept[name] ?? []), value] });
        }
        return { resolved: null } satisfies Resolved;
      }
      const [wait] = waiting.splice(at, 1);
      ctx.resolveAwakeable(wait!.awakeable, value);
      ctx.set('waiting', waiting);
      await publish(ctx);
      return { resolved: wait!.id } satisfies Resolved;
    },

    /** The waits, oldest first. */
    pending: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>): Promise<Waiting[]> =>
      ((await ctx.get('waiting')) ?? []).map(({ id, name, input, since }) => ({ id, name, input, since })),
    ),

    /** @internal A wait: resolved now with a signal kept for it, or else when one comes. */
    listen: internal(async (ctx: restate.ObjectContext<State>, wait: Waiting & { awakeable: string }) => {
      const kept = (await ctx.get('kept')) ?? {};
      const [value, ...rest] = kept[wait.name] ?? [];
      if (kept[wait.name]?.length) {
        ctx.resolveAwakeable(wait.awakeable, value);
        ctx.set('kept', { ...kept, [wait.name]: rest });
        return;
      }
      ctx.set('waiting', [...((await ctx.get('waiting')) ?? []), wait]);
      await publish(ctx);
    }),

    /**
     * @internal The agent's `chat` started answering `turn`: the same
     * invocation again (a retry) keeps when it started. A turn before it that
     * never closed (its invocation was killed) is over.
     */
    start: internal(async (ctx: restate.ObjectContext<State>, turn: Turn) => {
      if ((await ctx.get('turn'))?.invocationId !== turn.invocationId) ctx.set('turn', turn);
    }),

    /** What the agent does: the message it answers, if any, and the waits of its tools. */
    status: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>): Promise<Status> => ({
      answering: (await ctx.get('turn')) ?? null,
      waiting: ((await ctx.get('waiting')) ?? []).map(({ id, name, input, since }) => ({ id, name, input, since })),
    })),

    /**
     * The revision, once there is one newer than `revision`, or at the
     * timeout (50 s by default, at most 60 s), whichever comes first. It
     * waits in Restate, holding no process; a caller that retries it with the
     * same idempotency key joins the same wait.
     */
    watch: restate.handlers.object.shared(
      async (
        ctx: restate.ObjectSharedContext<State>,
        { revision, timeoutSeconds = 50 }: Watch,
      ): Promise<Notifications> => {
        const { id, promise } = ctx.awakeable<Notifications>();
        ctx.objectSendClient(signals, ctx.key).subscribe({ revision, awakeable: id });
        const timeout = ctx.sleep({ seconds: Math.min(Math.max(timeoutSeconds, 0), 60) }).map(() => null);
        const changed = await restate.RestatePromise.race([promise, timeout]);
        if (changed) return changed;
        ctx.objectSendClient(signals, ctx.key).unsubscribe(id);
        return { revision: (await ctx.get('revision')) ?? 0 };
      },
    ),

    /** @internal The message `turn` (an invocation id) is answered, or its turn ended otherwise. */
    close: internal(async (ctx: restate.ObjectContext<State>, turn: string) => {
      if ((await ctx.get('turn'))?.invocationId === turn) ctx.clear('turn');
    }),

    /**
     * @internal The agent forgot its conversation (`clear`): forget its
     * waits and kept signals, and publish the change.
     */
    reset: restate.handlers.object.exclusive({ ingressPrivate: true }, async (ctx: restate.ObjectContext<State>) => {
      ctx.clear('waiting');
      ctx.clear('kept');
      await publish(ctx);
    }),

    /** @internal Forget wait `id`, whose agent no longer waits. */
    forget: internal(async (ctx: restate.ObjectContext<State>, id: string) => {
      const waiting = (await ctx.get('waiting')) ?? [];
      ctx.set(
        'waiting',
        waiting.filter((w) => w.id !== id),
      );
      await publish(ctx);
    }),

    /** @internal The conversation changed: increase its revision, and answer the `watch`es. */
    publish: restate.handlers.object.exclusive({ ingressPrivate: true }, publish),

    /** The conversation's revision. */
    notifications: restate.handlers.object.shared(
      async (ctx: restate.ObjectSharedContext<State>): Promise<Notifications> => ({
        revision: (await ctx.get('revision')) ?? 0,
      }),
    ),

    /** @internal Answer `awakeable` once there is a revision newer than `revision`: at once, if there is. */
    subscribe: internal(async (ctx: restate.ObjectContext<State>, watch: { revision: number; awakeable: string }) => {
      const revision = (await ctx.get('revision')) ?? 0;
      if (revision > watch.revision) {
        ctx.resolveAwakeable(watch.awakeable, { revision });
        return;
      }
      ctx.set('watching', [...((await ctx.get('watching')) ?? []), watch]);
    }),

    /** @internal Forget the `watch` of `awakeable`, which no longer waits. */
    unsubscribe: internal(async (ctx: restate.ObjectContext<State>, awakeable: string) => {
      const watching = (await ctx.get('watching')) ?? [];
      ctx.set(
        'watching',
        watching.filter((w) => w.awakeable !== awakeable),
      );
    }),
  },
});

// Signals to agents: a virtual object per agent, keyed `<agent>:<key>` (such
// as `assistant:alice`), whose `wait` returns once `resolve` gives the wait
// its value; and the messages steered to the message an agent answers.
//
// An agent's own handler cannot wait on an awakeable (it has no journal to
// replay one from), so it calls `wait`, which can: a journaled handler of
// this object, which waits without holding a process.
import * as restate from '@restatedev/restate-sdk';

/** The key of the signals of the object `service`'s key `key`. */
export const signalsOf = (service: string, key: string) => `${service}:${key}`;

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

/** A message steered to the message being answered (its turn): its id, `<turn>:steer:<n>`, and its text. */
export interface Steer {
  id: string;
  text: string;
}

/**
 * What `steer` did: the invocation that answers it, and whether it steers
 * the message being answered (or, with none answered, was sent as a message
 * of its own).
 */
export interface Steered {
  invocationId: string;
  steered: boolean;
}

/** An agent: its object's name, and key. */
interface Agent {
  service: string;
  key: string;
}

/** The turns closed last, newest last: a message steered to one of them is a message of its own. */
const CLOSED = 16;

/**
 * The waits, in the order they started, each with its awakeable; the signals
 * kept for the next wait; and the messages steered to the turn being
 * answered, and the turns closed last.
 */
interface State {
  waiting: (Waiting & { awakeable: string })[];
  kept: Record<string, unknown[]>;
  steering: { turn: string; messages: Steer[] };
  closed: string[];
}

/** Send `text` to `agent`'s `chat`, as a message of its own: its invocation's id. */
const send = (ctx: restate.ObjectContext<State>, { service, key }: Agent, text: string) =>
  ctx.genericSend({ service, method: 'chat', key, parameter: text, inputSerde: restate.serde.json }).invocationId;

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
      return { resolved: wait!.id } satisfies Resolved;
    },

    /** The waits, oldest first. */
    pending: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>): Promise<Waiting[]> =>
      ((await ctx.get('waiting')) ?? []).map(({ id, name, input, since }) => ({ id, name, input, since })),
    ),

    /** @internal A wait: resolved now with a signal kept for it, or else when one comes. */
    listen: async (ctx: restate.ObjectContext<State>, wait: Waiting & { awakeable: string }) => {
      const kept = (await ctx.get('kept')) ?? {};
      const [value, ...rest] = kept[wait.name] ?? [];
      if (kept[wait.name]?.length) {
        ctx.resolveAwakeable(wait.awakeable, value);
        ctx.set('kept', { ...kept, [wait.name]: rest });
        return;
      }
      ctx.set('waiting', [...((await ctx.get('waiting')) ?? []), wait]);
    },

    /**
     * @internal Steer `turn` (the message `agent` answers, if any) with
     * `text`: queued for it to take at its next step, or, if it is over, sent
     * as a message of its own.
     */
    steer: async (
      ctx: restate.ObjectContext<State>,
      { agent, turn, text }: { agent: Agent; turn: string | null; text: string },
    ) => {
      if (turn === null || ((await ctx.get('closed')) ?? []).includes(turn)) {
        return { invocationId: await send(ctx, agent, text), steered: false } satisfies Steered;
      }
      const steering = await ctx.get('steering');
      const messages = steering?.turn === turn ? steering.messages : [];
      ctx.set('steering', { turn, messages: [...messages, { id: `${turn}:steer:${messages.length + 1}`, text }] });
      return { invocationId: turn, steered: true } satisfies Steered;
    },

    /** @internal The messages steered to `turn`, in order. */
    steering: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>, turn: string) => {
      const steering = await ctx.get('steering');
      return steering?.turn === turn ? steering.messages : [];
    }),

    /**
     * @internal `turn` is over: each message steered to it that it did not
     * take (not in `taken`) is sent as a message of its own, unless the turn
     * was cancelled; later ones are, too.
     */
    close: async (
      ctx: restate.ObjectContext<State>,
      { agent, turn, taken, cancelled }: { agent: Agent; turn: string; taken: string[]; cancelled: boolean },
    ) => {
      const closed = (await ctx.get('closed')) ?? [];
      if (closed.includes(turn)) return;
      ctx.set('closed', [...closed, turn].slice(-CLOSED));
      const steering = await ctx.get('steering');
      if (steering?.turn !== turn) return;
      ctx.clear('steering');
      if (cancelled) return;
      for (const { id, text } of steering.messages) if (!taken.includes(id)) await send(ctx, agent, text);
    },

    /** @internal Forget wait `id`, whose agent no longer waits. */
    forget: async (ctx: restate.ObjectContext<State>, id: string) => {
      const waiting = (await ctx.get('waiting')) ?? [];
      ctx.set(
        'waiting',
        waiting.filter((w) => w.id !== id),
      );
    },
  },
});

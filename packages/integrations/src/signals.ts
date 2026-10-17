// Signals to agents: a virtual object per agent, keyed `<agent>:<key>` (such
// as `assistant:alice`), whose `wait(name)` returns once `resolve({ name,
// value })` gives the signal its value.
//
// An agent's own handler cannot wait on an awakeable (it has no journal to
// replay one from), so it calls `wait`, which can: a journaled handler of
// this object, which waits without holding a process. A signal resolved
// before anyone waits is kept for the next `wait`.
import * as restate from '@restatedev/restate-sdk';

/** The key of the signals of the object `service`'s key `key`. */
export const signalsOf = (service: string, key: string) => `${service}:${key}`;

/** The awakeables waiting for each signal, and the values resolved before anyone waited. */
interface State {
  waiting: Record<string, string[]>;
  early: Record<string, unknown[]>;
}

export const signals = restate.object({
  name: 'signals',
  handlers: {
    /** Wait for the signal `name`, and return its value. */
    wait: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>, name: string) => {
      const { id, promise } = ctx.awakeable<unknown>();
      ctx.objectSendClient(signals, ctx.key).listen({ name, awakeable: id });
      return promise;
    }),

    /** Resolve the signal `name` with `value`: for whoever waits for it, or else the next to. */
    resolve: async (ctx: restate.ObjectContext<State>, { name, value }: { name: string; value?: unknown }) => {
      const waiting = (await ctx.get('waiting')) ?? {};
      const awakeables = waiting[name] ?? [];
      if (awakeables.length === 0) {
        const early = (await ctx.get('early')) ?? {};
        ctx.set('early', { ...early, [name]: [...(early[name] ?? []), value ?? null] });
        return;
      }
      for (const awakeable of awakeables) ctx.resolveAwakeable(awakeable, value ?? null);
      delete waiting[name];
      ctx.set('waiting', waiting);
    },

    /** The signals waited for. */
    pending: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext<State>) =>
      Object.keys((await ctx.get('waiting')) ?? {}),
    ),

    /** @internal Who waits for `name`: resolved now with a value resolved before, or else when one comes. */
    listen: async (ctx: restate.ObjectContext<State>, { name, awakeable }: { name: string; awakeable: string }) => {
      const early = (await ctx.get('early')) ?? {};
      const [value, ...rest] = early[name] ?? [];
      if (early[name]?.length) {
        ctx.resolveAwakeable(awakeable, value);
        ctx.set('early', { ...early, [name]: rest });
        return;
      }
      const waiting = (await ctx.get('waiting')) ?? {};
      ctx.set('waiting', { ...waiting, [name]: [...(waiting[name] ?? []), awakeable] });
    },
  },
});

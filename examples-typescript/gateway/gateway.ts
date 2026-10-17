// The tool gateway: one door to every tool, for the agents (agent.ts). A
// Gateway virtual object per tenant
// (`Gateway/acme`), with the journal.
//
// - `tools()`: the prebuilt tools (see tools.ts).
// - `call({ user, name, arguments })` runs one:
//   1. approval, if the tool requires it: the call waits on an awakeable,
//      held in the object's state, until a manager decides (`pending`,
//      `decide`). Meanwhile it is suspended, and costs nothing;
//   2. the call, in the tenant's scope, with `<tool>/<user>` as limit key.
//      The server's rules hold the calls over a limit in a queue: per
//      tenant (`*/lookup_order`) and per user (`*/lookup_order/*`). Each
//      user's calls have a queue of their own, and the queues take turns.
//   A tool's failure is a result with `isError`, for the model to read.
//
// `tools`, `call` and `pending` are shared: a tenant's calls run at once,
// and one waiting for approval holds up no other.
//
// Environment: PORT.
import * as restate from '@restatedev/restate-sdk';
import { PREBUILT } from './tools.ts';

/** A tool, as a model sees it, and the handler that runs it. */
export interface Tool {
  name: string;
  description: string;
  inputSchema: object;
  approval: boolean;
  service: string;
  handler: string;
}

/** A tool's result, as a model reads it. */
export interface ToolResult {
  content: { type: 'text'; text: string }[];
  isError?: boolean;
}

/** A tool call of a tenant's user. */
export interface CallRequest {
  user: string;
  name: string;
  arguments: Record<string, unknown>;
}

/** A call held for approval, until `decide` resolves its awakeable. */
export interface Held extends CallRequest {
  awakeable: string;
}

/** The tenant and the user of `<tenant>.<user>` (an agent's key). */
export function identity(of: string): { tenant: string; user: string } {
  const [tenant = '', user = 'anonymous'] = of.split('.', 2);
  return { tenant, user };
}

const HELD = 'held';

const result = (text: string, isError = false): ToolResult => ({
  content: [{ type: 'text', text }],
  ...(isError ? { isError } : {}),
});

export const gateway = restate.object({
  name: 'Gateway',
  handlers: {
    /** The tools. */
    tools: restate.handlers.object.shared(async (): Promise<Tool[]> => Object.values(PREBUILT)),

    call: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext, req: CallRequest) => {
      const tool = PREBUILT[req.name];
      if (!tool) return result(`${ctx.key} has no tool ${req.name}`, true);
      if (tool.approval) {
        const { id, promise } = ctx.awakeable<boolean>();
        ctx.genericSend<Held>({
          service: 'Gateway',
          key: ctx.key,
          method: 'hold',
          parameter: { ...req, awakeable: id },
          inputSerde: restate.serde.json,
        });
        if (!(await promise)) return result(`${tool.name} was not approved`, true);
      }
      try {
        const output = await ctx.genericCall({
          service: tool.service,
          method: tool.handler,
          parameter: req.arguments,
          inputSerde: restate.serde.json,
          outputSerde: restate.serde.json,
          scope: ctx.key,
          limitKey: `${tool.name}/${req.user}`,
          headers: { 'x-on-behalf-of': req.user },
        });
        return result(JSON.stringify(output));
      } catch (e) {
        if (e instanceof restate.TerminalError) return result(e.message, true);
        throw e;
      }
    }),

    hold: async (ctx: restate.ObjectContext, held: Held) => {
      ctx.set(HELD, [...((await ctx.get<Held[]>(HELD)) ?? []), held]);
    },

    /** The calls waiting for a decision. */
    pending: restate.handlers.object.shared(
      async (ctx: restate.ObjectSharedContext) => (await ctx.get<Held[]>(HELD)) ?? [],
    ),

    /** Approve or deny a held call. */
    decide: async (ctx: restate.ObjectContext, { awakeable, approved }: { awakeable: string; approved: boolean }) => {
      const held = (await ctx.get<Held[]>(HELD)) ?? [];
      if (!held.some((h) => h.awakeable === awakeable))
        throw new restate.TerminalError(`no call waits on ${awakeable}`);
      ctx.set(
        HELD,
        held.filter((h) => h.awakeable !== awakeable),
      );
      ctx.resolveAwakeable(awakeable, approved);
      return approved;
    },
  },
  // A waiting call suspends soon, and holds no process.
  options: { inactivityTimeout: { seconds: 1 }, journalRetention: { hours: 1 } },
});

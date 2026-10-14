// The tool gateway: one door to every tool, for an agent's RPC (agent.ts) and
// for MCP clients (mcp.ts). A Gateway virtual object per tenant
// (`Gateway/acme`), with the journal.
//
// - `tools()`: the handlers whose metadata names a tool (see tools.ts), from
//   the admin API, that the tenant may use.
// - `call({ user, name, arguments })` runs one:
//   1. policy: a tool the tenant may not use is denied;
//   2. approval, if the tool requires it: the call waits on an awakeable,
//      held in the object's state, until a manager decides (`pending`,
//      `decide`). Meanwhile it is suspended, and costs nothing;
//   3. the call, in the tenant's scope, with `<tool>/<user>` as limit key.
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
import { serveIfMain } from '../faults.ts';

/** The tools each tenant may use; a tenant not listed has none. */
export const POLICY: Record<string, string[]> = {
  acme: ['lookup_order', 'refund'],
  globex: ['lookup_order'],
};

/** A tool, as MCP lists it, and the handler that runs it. */
export interface Tool {
  name: string;
  description: string;
  inputSchema: object;
  approval: boolean;
  service: string;
  handler: string;
}

/** An MCP tool result. */
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

/** The tenant and the user of `<tenant>.<user>` (an agent's key, an MCP token). */
export function identity(of: string): { tenant: string; user: string } {
  const [tenant = '', user = 'anonymous'] = of.split('.', 2);
  return { tenant, user };
}

type AdminServices = {
  services: {
    name: string;
    handlers: { name: string; documentation?: string; metadata?: Record<string, string>; input_json_schema?: object }[];
  }[];
};

/** The tools among the services of the admin API's `GET /services`. */
export const toolsOf = ({ services }: AdminServices): Tool[] =>
  services.flatMap((s) =>
    s.handlers.flatMap((h) => {
      const name = h.metadata?.['mcp.tool'];
      if (!name) return [];
      const { $schema: _, ...inputSchema } = (h.input_json_schema ?? { type: 'object' }) as Record<string, unknown>;
      const approval = h.metadata?.['mcp.approval'] === 'required';
      return [{ name, description: h.documentation ?? '', inputSchema, approval, service: s.name, handler: h.name }];
    }),
  );

const registry = async () => toolsOf((await (await fetch('http://127.0.0.1:9070/services')).json()) as AdminServices);

const allowed = (tenant: string, tool: string) => POLICY[tenant]?.includes(tool) ?? false;

const result = (text: string, isError = false): ToolResult => ({
  content: [{ type: 'text', text }],
  ...(isError ? { isError } : {}),
});

const HELD = 'held';

export const gateway = restate.object({
  name: 'Gateway',
  handlers: {
    tools: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext): Promise<Tool[]> =>
      (await ctx.run('registry', registry)).filter((t) => allowed(ctx.key, t.name)),
    ),

    call: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext, req: CallRequest) => {
      const tool = (await ctx.run('registry', registry)).find((t) => t.name === req.name);
      if (!tool || !allowed(ctx.key, tool.name)) return result(`${ctx.key} has no tool ${req.name}`, true);
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
  options: { inactivityTimeout: { seconds: 1 } },
});

serveIfMain(import.meta, [gateway]);

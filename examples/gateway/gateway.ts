// The tool gateway: one door to every tool, for an agent's RPC (agent.ts) and
// for MCP clients (mcp.ts). A plain Restate service, with the journal.
//
// - `tools({ tenant })`: the handlers whose metadata names a tool (see
//   tools.ts), from the admin API, that the tenant may use.
// - `call({ tenant, user, name, arguments })` runs one:
//   1. policy: a tool the tenant may not use is denied;
//   2. approval, if the tool requires it: the call waits on an awakeable,
//      held in the tenant's object, until a manager decides. Meanwhile it is
//      suspended, and costs nothing;
//   3. the call, in the tenant's scope, with `<tool>/<user>` as limit key.
//      The server's rules hold the calls over a limit in a queue: per
//      tenant (`*/lookup_order`) and per user (`*/lookup_order/*`). Each
//      user's calls have a queue of their own, and the queues take turns;
//   4. a record in the tenant's audit log.
//   A tool's failure is a result with `isError`, for the model to read.
//
// The journal makes each step once. The caller makes `call` once, with an
// idempotency key.
//
// Environment: PORT.
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from '../faults.ts';
import { tenant as tenantObject, type CallRecord } from './tenant.ts';

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

export interface CallRequest {
  tenant: string;
  user: string;
  name: string;
  arguments: Record<string, unknown>;
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

export const gateway = restate.service({
  name: 'Gateway',
  handlers: {
    tools: async (ctx: restate.Context, { tenant }: { tenant: string }): Promise<Tool[]> =>
      (await ctx.run('registry', registry)).filter((t) => allowed(tenant, t.name)),

    call: async (ctx: restate.Context, req: CallRequest): Promise<ToolResult> => {
      const started = await ctx.date.now();
      const run = async (): Promise<Pick<CallRecord, 'outcome' | 'detail'>> => {
        const tool = (await ctx.run('registry', registry)).find((t) => t.name === req.name);
        if (!tool || !allowed(req.tenant, tool.name)) {
          return { outcome: 'denied', detail: `${req.tenant} has no tool ${req.name}` };
        }
        if (tool.approval) {
          const { id, promise } = ctx.awakeable<boolean>();
          ctx
            .objectSendClient(tenantObject, req.tenant)
            .hold({ awakeable: id, user: req.user, tool: tool.name, arguments: req.arguments });
          if (!(await promise)) return { outcome: 'denied', detail: `${tool.name} was not approved` };
        }
        try {
          const result = await ctx.genericCall({
            service: tool.service,
            method: tool.handler,
            parameter: req.arguments,
            inputSerde: restate.serde.json,
            outputSerde: restate.serde.json,
            scope: req.tenant,
            limitKey: `${tool.name}/${req.user}`,
            headers: { 'x-on-behalf-of': req.user },
          });
          return { outcome: 'ok', detail: JSON.stringify(result) };
        } catch (e) {
          if (e instanceof restate.TerminalError) return { outcome: 'error', detail: e.message };
          throw e;
        }
      };
      const { outcome, detail } = await run();
      const ms = (await ctx.date.now()) - started;
      ctx
        .objectSendClient(tenantObject, req.tenant)
        .record({ call: ctx.request().id, user: req.user, tool: req.name, outcome, detail, ms });
      return { content: [{ type: 'text', text: detail }], ...(outcome === 'ok' ? {} : { isError: true }) };
    },
  },
  // A waiting call suspends soon, and holds no process.
  options: { inactivityTimeout: { seconds: 1 } },
});

serveIfMain(import.meta, [gateway, tenantObject]);

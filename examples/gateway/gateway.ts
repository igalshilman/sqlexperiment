// The tool gateway: one door to every tool, for an agent's RPC (agent.ts) and
// for MCP clients (mcp.ts). A plain Restate service, with the journal.
//
// - `tools({ tenant })` is the registry: the handlers whose metadata names a
//   tool (`mcp.tool`, see tools.ts), read from the admin API, that the
//   tenant's policy allows. Deploy a service with tools, and they are listed.
// - `call({ tenant, caller, name, arguments })` runs one:
//   1. the policy: a tool the tenant may not use is refused;
//   2. approval, for a tool that requires it (`mcp.approval: required`): the
//      call is held on an awakeable, recorded in the tenant's object, and the
//      manager paged, until a decision (Tenant.decide) or a timeout. It costs
//      nothing meanwhile: the invocation is suspended, and the agent's too;
//   3. the call, in the tenant's scope, with `<tool>/<user>` as limit key:
//      the server's limit rules (`*/lookup_order` at 2, say) hold the calls
//      over the limit in the scope's queue until one ends, so the upstream
//      API never sees more than its limit, and one tenant's burst does not
//      queue another tenant's calls. Each user's calls queue apart and take
//      turns, so one user's burst does not hold up another's either; and a
//      rule on `*/lookup_order/*` caps what each user runs at once;
//   4. a record of the call in the tenant's audit log (tenant.ts).
//
//   It returns an MCP tool result: a tool's failure (a terminal error) is a
//   result with `isError`, for the model to read, not a failed call.
//
// The journal makes each step once: a retried attempt of `call` does not ask
// again for approval, call the tool again, or record it twice. The caller
// makes `call` once with an idempotency key (agent.ts, mcp.ts).
//
// Environment: RESTATE_ADMIN_URL (default http://127.0.0.1:9070),
// UPSTREAM_URL (the pager, default http://127.0.0.1:9300),
// APPROVAL_TIMEOUT_MS (default 10 minutes) and PORT.
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from '../faults.ts';
import { tenant as tenantObject, type Decision, type Outcome } from './tenant.ts';

const ADMIN = process.env.RESTATE_ADMIN_URL ?? 'http://127.0.0.1:9070';
const UPSTREAM = process.env.UPSTREAM_URL ?? 'http://127.0.0.1:9300';
const APPROVAL_TIMEOUT_MS = Number(process.env.APPROVAL_TIMEOUT_MS ?? 10 * 60_000);

/** The tools each tenant may use; a tenant not listed has none. */
export const POLICY: Record<string, '*' | string[]> = {
  acme: '*',
  globex: ['lookup_order', 'email_customer'],
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
  /** The tenant's user on whose behalf the tool runs: the second level of its limit key. */
  user: string;
  /** Who calls, for the audit log: an agent's key, an MCP session. */
  caller: string;
  name: string;
  arguments: Record<string, unknown>;
}

/** A handler, as the admin API describes it. */
export interface AdminHandler {
  name: string;
  documentation?: string;
  metadata?: Record<string, string>;
  input_json_schema?: object;
}

/** The tools among the services of the admin API's `GET /services`. */
export function toolsOf({ services }: { services: { name: string; handlers: AdminHandler[] }[] }): Tool[] {
  return services.flatMap((service) =>
    service.handlers.flatMap((h) => {
      const name = h.metadata?.['mcp.tool'];
      if (!name) return [];
      // MCP's input schema: the handler's, without the dialect line.
      const { $schema: _, ...inputSchema } = (h.input_json_schema ?? { type: 'object' }) as Record<string, unknown>;
      return [
        {
          name,
          description: h.documentation ?? '',
          inputSchema,
          approval: h.metadata?.['mcp.approval'] === 'required',
          service: service.name,
          handler: h.name,
        },
      ];
    }),
  );
}

/** Every tool deployed, from the admin API. */
async function registry(): Promise<Tool[]> {
  const res = await fetch(`${ADMIN}/services`);
  if (!res.ok) throw new Error(`admin API: ${res.status}`);
  return toolsOf((await res.json()) as Parameters<typeof toolsOf>[0]);
}

/** The tenant and the user of `<tenant>.<user>` (an agent's key, an MCP token); a user of `anonymous` without one. */
export function identity(of: string): { tenant: string; user: string } {
  const dot = of.indexOf('.');
  return dot < 0 ? { tenant: of, user: 'anonymous' } : { tenant: of.slice(0, dot), user: of.slice(dot + 1) };
}

/** What a scope or a limit key's level may be. */
const KEY = /^[\w.-]{1,36}$/;

const allowed = (tenant: string, tool: string) => {
  const policy = POLICY[tenant];
  return policy === '*' || (policy?.includes(tool) ?? false);
};

const text = (value: unknown) => (typeof value === 'string' ? value : JSON.stringify(value));

export const gateway = restate.service({
  name: 'Gateway',
  handlers: {
    /** The tools `tenant` may use. */
    tools: async (ctx: restate.Context, { tenant }: { tenant: string }): Promise<Tool[]> =>
      (await ctx.run('registry', registry)).filter((t) => allowed(tenant, t.name)),

    /** Run tool `name` for `tenant`: policy, approval, the call in the tenant's scope, and the audit log. */
    call: async (ctx: restate.Context, req: CallRequest): Promise<ToolResult> => {
      const started = await ctx.date.now();
      const { outcome, detail } = await (async (): Promise<{ outcome: Outcome; detail: string }> => {
        const tool = (await ctx.run('registry', registry)).find((t) => t.name === req.name);
        if (!tool || !allowed(req.tenant, tool.name)) {
          return { outcome: 'denied', detail: `${req.tenant} has no tool ${req.name}` };
        }
        if (!KEY.test(req.user)) return { outcome: 'denied', detail: `no user ${JSON.stringify(req.user)}` };
        if (tool.approval) {
          const { id, promise } = ctx.awakeable<Decision>();
          const held = {
            awakeable: id,
            call: ctx.request().id,
            at: started,
            user: req.user,
            caller: req.caller,
            tool: tool.name,
          };
          ctx.objectSendClient(tenantObject, req.tenant).hold({ ...held, arguments: req.arguments });
          await ctx.run('page the manager', async () => {
            const res = await fetch(`${UPSTREAM}/pages`, {
              method: 'POST',
              headers: { 'content-type': 'application/json' },
              body: JSON.stringify({ tenant: req.tenant, tool: tool.name, arguments: req.arguments, awakeable: id }),
            });
            if (!res.ok) throw new Error(`pager: ${res.status}`);
          });
          const decision = await promise.orTimeout(APPROVAL_TIMEOUT_MS).catch((e: unknown) => {
            if (e instanceof restate.TimeoutError) return { approved: false, by: 'nobody, in time' };
            throw e;
          });
          if (!decision.approved)
            return { outcome: 'rejected', detail: `${tool.name} was not approved by ${decision.by}` };
        }
        try {
          const result = await ctx.genericCall({
            service: tool.service,
            method: tool.handler,
            parameter: req.arguments,
            inputSerde: restate.serde.json,
            outputSerde: restate.serde.json,
            scope: req.tenant,
            // Two levels: the tool, then the user. A rule on `*/<tool>`
            // limits the tenant's calls, one on `*/<tool>/*` each user's.
            limitKey: `${tool.name}/${req.user}`,
            headers: { 'x-on-behalf-of': req.user },
          });
          return { outcome: 'ok', detail: text(result) };
        } catch (e) {
          if (e instanceof restate.TerminalError) return { outcome: 'error', detail: e.message };
          throw e;
        }
      })();
      const ended = await ctx.date.now();
      ctx.objectSendClient(tenantObject, req.tenant).record({
        call: ctx.request().id,
        at: started,
        user: req.user,
        caller: req.caller,
        tool: req.name,
        arguments: req.arguments,
        outcome,
        detail,
        ms: ended - started,
      });
      return outcome === 'ok'
        ? { content: [{ type: 'text', text: detail }] }
        : { content: [{ type: 'text', text: detail }], isError: true };
    },
  },
  options: {
    // A call waiting (for approval, for a tool) suspends soon: it holds no
    // process, and a restart of the gateway's process does not disturb it.
    inactivityTimeout: { seconds: 1 },
  },
});

serveIfMain(import.meta, [gateway, tenantObject]);

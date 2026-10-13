// The tools: plain Restate services, with the journal, as any service is
// written. Nothing in them knows about SQL, agents or MCP.
//
// A handler is a tool when its metadata says so (`mcp.tool`, the tool's
// name). The gateway finds the tools by reading the admin API, as an MCP
// client's `tools/list`: its description is the handler's, and its input
// schema the JSON schema of the handler's zod input. `mcp.approval:
// required` makes the gateway hold a call until a person approves it.
//
// The gateway calls a tool in the tenant's scope, and a tool knows its tenant
// from the scope (`ctx.request().scope`), not from its arguments: it is the
// API key it uses with the outside world (../gateway/upstream.ts). The user
// it runs for comes in a header (`x-on-behalf-of`), which it passes on.
//
// Each step that reaches the outside world is a `ctx.run`: retried until it
// succeeds (a 429, a lost connection), and journaled once it has, so a
// retried invocation does not make it again. A refund and a mail carry an
// idempotency key made from the invocation id, the same on every attempt.
//
// Environment: UPSTREAM_URL (default http://127.0.0.1:9300) and PORT.
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';

const UPSTREAM = process.env.UPSTREAM_URL ?? 'http://127.0.0.1:9300';

/** A request to the outside world, as the tenant: a 429 or a 5xx is retried, any other failure is terminal. */
async function api<T>(
  ctx: restate.Context,
  path: string,
  init: { method?: string; headers?: Record<string, string>; body?: string } = {},
): Promise<T> {
  const res = await fetch(`${UPSTREAM}${path}`, {
    ...init,
    headers: {
      'content-type': 'application/json',
      'x-tenant': ctx.request().scope ?? 'anonymous',
      'x-user': ctx.request().headers.get('x-on-behalf-of') ?? 'anonymous',
      ...init.headers,
    },
  });
  if (res.status === 429 || res.status >= 500) throw new Error(`${path}: ${res.status} ${await res.text()}`);
  if (!res.ok) throw new restate.TerminalError(`${path}: ${res.status} ${await res.text()}`);
  return (await res.json()) as T;
}

const tool = (name: string, extra: Record<string, string> = {}) => ({ 'mcp.tool': name, ...extra });

const LookupOrder = z.object({ order: z.string().describe('The order id, such as "101"') });
const Refund = z.object({
  order: z.string().describe('The order id'),
  amount: z.number().int().positive().describe('The amount to refund, in dollars'),
  reason: z.string().describe('Why the customer is refunded'),
});
const EmailCustomer = z.object({
  order: z.string().describe('The order whose customer to write to'),
  text: z.string().describe('The text of the mail'),
});

export const orders = restate.service({
  name: 'Orders',
  handlers: {
    lookup: restate.handlers.handler(
      {
        description: 'Look up an order: its status, customer and total.',
        metadata: tool('lookup_order'),
        input: restate.serde.schema(LookupOrder),
      },
      async (ctx: restate.Context, { order }: z.infer<typeof LookupOrder>) =>
        ctx.run('GET /orders', () => api<Record<string, unknown>>(ctx, `/orders/${encodeURIComponent(order)}`)),
    ),
  },
});

export const payments = restate.service({
  name: 'Payments',
  handlers: {
    refund: restate.handlers.handler(
      {
        description: 'Refund an amount of an order to its customer. A manager approves each refund.',
        metadata: tool('refund', { 'mcp.approval': 'required' }),
        input: restate.serde.schema(Refund),
      },
      async (ctx: restate.Context, { order, amount, reason }: z.infer<typeof Refund>) => {
        const known = await ctx.run('GET /orders', () =>
          api<{ total: number }>(ctx, `/orders/${encodeURIComponent(order)}`),
        );
        if (amount > known.total) {
          throw new restate.TerminalError(`order ${order} totals ${known.total}: it cannot be refunded ${amount}`);
        }
        // Once per invocation, however many attempts it takes.
        const idempotencyKey = ctx.request().id;
        return ctx.run('POST /refunds', () =>
          api<{ refund: string }>(ctx, '/refunds', {
            method: 'POST',
            headers: { 'idempotency-key': idempotencyKey },
            body: JSON.stringify({ order, amount, reason }),
          }),
        );
      },
    ),
  },
});

export const mail = restate.service({
  name: 'Mail',
  handlers: {
    emailCustomer: restate.handlers.handler(
      {
        description: "Send a mail to an order's customer.",
        metadata: tool('email_customer'),
        input: restate.serde.schema(EmailCustomer),
      },
      async (ctx: restate.Context, { order, text }: z.infer<typeof EmailCustomer>) => {
        const { customer } = await ctx.run('GET /orders', () =>
          api<{ customer: string }>(ctx, `/orders/${encodeURIComponent(order)}`),
        );
        const idempotencyKey = ctx.request().id;
        await ctx.run('POST /mail', () =>
          api(ctx, '/mail', {
            method: 'POST',
            headers: { 'idempotency-key': idempotencyKey },
            body: JSON.stringify({ to: customer, text }),
          }),
        );
        return { sent: true, to: customer };
      },
    ),
  },
});

export const tools = [orders, payments, mail];

serveIfMain(import.meta, tools);

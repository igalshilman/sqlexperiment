// The tools: plain Restate services, with the journal. Nothing in them knows
// about SQL, agents or MCP.
//
// A handler is a tool when its metadata says so (`mcp.tool`); its
// description and the JSON schema of its zod input are the tool's. The
// gateway calls it in the tenant's scope, so a tool knows its tenant from
// `ctx.request().scope`, and the user it runs for from a header.
//
// Each request to the outside world is a `ctx.run`: retried until it
// succeeds (a 429 included), and journaled once it has. A refund carries the
// invocation id as idempotency key, the same on every attempt.
//
// Environment: PORT, UPSTREAM_URL (http://127.0.0.1:9300).
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';

const UPSTREAM = process.env.UPSTREAM_URL ?? 'http://127.0.0.1:9300';

/** A request to the outside world (upstream.ts), as the tenant and the user: a 429 is retried, any other failure is terminal. */
async function api<T>(ctx: restate.Context, path: string, method = 'GET', headers: Record<string, string> = {}) {
  const res = await fetch(`${UPSTREAM}${path}`, {
    method,
    headers: {
      'x-tenant': ctx.request().scope ?? '',
      'x-user': ctx.request().headers.get('x-on-behalf-of') ?? '',
      ...headers,
    },
  });
  if (res.status === 429) throw new Error(`${path}: 429`);
  if (!res.ok) throw new restate.TerminalError(`${path}: ${res.status}`);
  return (await res.json()) as T;
}

const LookupOrder = z.object({ order: z.string().describe('The order id, such as "101"') });
const Refund = z.object({
  order: z.string().describe('The order id'),
  amount: z.number().int().positive().describe('The amount to refund, in dollars'),
});

export const orders = restate.service({
  name: 'Orders',
  handlers: {
    lookup: restate.handlers.handler(
      {
        description: 'Look up an order: its status and total.',
        metadata: { 'mcp.tool': 'lookup_order' },
        input: restate.serde.schema(LookupOrder),
      },
      async (ctx: restate.Context, { order }: z.infer<typeof LookupOrder>) =>
        ctx.run('GET /orders', () => api<object>(ctx, `/orders/${encodeURIComponent(order)}`)),
    ),
  },
});

export const payments = restate.service({
  name: 'Payments',
  handlers: {
    refund: restate.handlers.handler(
      {
        description: 'Refund an amount of an order. A manager approves each refund.',
        metadata: { 'mcp.tool': 'refund', 'mcp.approval': 'required' },
        input: restate.serde.schema(Refund),
      },
      async (ctx: restate.Context, { order, amount }: z.infer<typeof Refund>) => {
        const { total } = await ctx.run('GET /orders', () =>
          api<{ total: number }>(ctx, `/orders/${encodeURIComponent(order)}`),
        );
        if (amount > total) throw new restate.TerminalError(`order ${order} totals ${total}, less than ${amount}`);
        const idempotencyKey = ctx.request().id;
        return ctx.run('POST /refunds', () =>
          api<{ refund: string }>(ctx, '/refunds', 'POST', { 'idempotency-key': idempotencyKey }),
        );
      },
    ),
  },
});

export const tools = [orders, payments];

serveIfMain(import.meta, tools);

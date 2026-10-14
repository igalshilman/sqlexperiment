// The tools: plain Restate services, with the journal. Nothing in them knows
// about SQL or agents.
//
// PREBUILT is the gateway's tools: each one's handler, and the JSON schema of
// its zod input. The gateway calls a tool in the tenant's scope, so a
// tool knows its tenant from `ctx.request().scope`, and the user it runs for
// from a header.
//
// The outside world is faked in this process: an order system that is slow
// and serves at most 2 lookups at once per tenant (one more fails, like a
// 429), and a payment provider with one refund per idempotency key. Each
// request to it is a `ctx.run`: retried until it succeeds, and journaled
// once it has.
//
// Environment: PORT.
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import type { Tool } from './gateway.ts';

/** Order `id`: made up, the same every time. */
const orderOf = (id: string) => {
  const n = id.split('').reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 1009, 7);
  const status = ['shipped', 'delayed at customs', 'waiting for stock', 'delivered'][n % 4]!;
  return { id, status, total: 20 + (n % 80) };
};

const inFlight = new Map<string, number>();

/** The fake order system: 400 ms a lookup, and at most 2 at once per tenant. */
async function fetchOrder(tenant: string, id: string) {
  const n = inFlight.get(tenant) ?? 0;
  if (n >= 2) throw new Error('order system: 429, too many requests'); // retried
  inFlight.set(tenant, n + 1);
  try {
    await new Promise((r) => setTimeout(r, 400));
    return orderOf(id);
  } finally {
    inFlight.set(tenant, inFlight.get(tenant)! - 1);
  }
}

const refunds: string[] = [];
const tickets: string[] = [];

/** The fake payment provider: one refund per idempotency key, as Stripe does. */
const makeRefund = async (idempotencyKey: string) => {
  if (!refunds.includes(idempotencyKey)) refunds.push(idempotencyKey);
  return { refund: `re_${refunds.indexOf(idempotencyKey)}` };
};

/** The fake ticket system. */
const fileTicket = async (idempotencyKey: string) => {
  if (!tickets.includes(idempotencyKey)) tickets.push(idempotencyKey);
  return { ticket: `T-${1000 + tickets.indexOf(idempotencyKey)}` };
};

const LookupOrder = z.object({ order: z.string().describe('The order id, such as "101"') });
const Refund = z.object({
  order: z.string().describe('The order id'),
  amount: z.number().int().positive().describe('The amount to refund, in dollars'),
});
const OpenTicket = z.object({
  order: z.string().describe('The order id'),
  summary: z.string().describe("The customer's problem, in a sentence"),
});

const DESCRIPTION = {
  lookup: 'Look up an order: its status and total.',
  refund: 'Refund an amount of an order. A manager approves each refund.',
  ticket: 'Open a support ticket about an order, for a person to follow up.',
};

export const orders = restate.service({
  name: 'Orders',
  handlers: {
    lookup: restate.handlers.handler(
      {
        description: DESCRIPTION.lookup,
        input: restate.serde.schema(LookupOrder),
      },
      async (ctx: restate.Context, { order }: z.infer<typeof LookupOrder>) =>
        ctx.run('GET /orders', () => fetchOrder(ctx.request().scope ?? '', order)),
    ),
  },
});

export const payments = restate.service({
  name: 'Payments',
  handlers: {
    refund: restate.handlers.handler(
      {
        description: DESCRIPTION.refund,
        input: restate.serde.schema(Refund),
      },
      async (ctx: restate.Context, { order, amount }: z.infer<typeof Refund>) => {
        const { total } = await ctx.run('GET /orders', () => fetchOrder(ctx.request().scope ?? '', order));
        if (amount > total) throw new restate.TerminalError(`order ${order} totals ${total}, less than ${amount}`);
        const idempotencyKey = ctx.request().id; // the same on every attempt
        return ctx.run('POST /refunds', () => makeRefund(idempotencyKey));
      },
    ),
  },
});

export const support = restate.service({
  name: 'Support',
  handlers: {
    ticket: restate.handlers.handler(
      { description: DESCRIPTION.ticket, input: restate.serde.schema(OpenTicket) },
      async (ctx: restate.Context, { order, summary }: z.infer<typeof OpenTicket>) => {
        const idempotencyKey = ctx.request().id;
        const { ticket } = await ctx.run('POST /tickets', () => fileTicket(idempotencyKey));
        return { ticket, order, summary, status: 'open' };
      },
    ),
  },
});

export const tools = [orders, payments, support];

const schema = (input: z.ZodType) => {
  const { $schema: _, ...json } = z.toJSONSchema(input) as Record<string, unknown>;
  return json;
};

/** The prebuilt tools, by name. */
export const PREBUILT: Record<string, Tool> = {
  lookup_order: {
    name: 'lookup_order',
    description: DESCRIPTION.lookup,
    inputSchema: schema(LookupOrder),
    approval: false,
    service: 'Orders',
    handler: 'lookup',
  },
  refund: {
    name: 'refund',
    description: DESCRIPTION.refund,
    inputSchema: schema(Refund),
    approval: true,
    service: 'Payments',
    handler: 'refund',
  },
  open_ticket: {
    name: 'open_ticket',
    description: DESCRIPTION.ticket,
    inputSchema: schema(OpenTicket),
    approval: false,
    service: 'Support',
    handler: 'ticket',
  },
};

serveIfMain(import.meta, tools);

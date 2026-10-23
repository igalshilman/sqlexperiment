/**
 * A support agent whose tools are Restate itself: it looks orders up with a
 * plain service, waits for a manager's approval (a signal), and refunds with
 * a workflow. Every step is durable: a crash, or a manager who takes a day,
 * changes nothing.
 *
 *   ANTHROPIC_API_KEY=… node intermediate/support.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/support/alice/chat -d 'Order 103 arrived broken, can I get my $30 back?'
 *
 * The agent waits for approval; in another terminal, see what it waits for
 * (the refund, with the wait's id), and approve it:
 *
 *   curl localhost:8080/support/alice/pending
 *   curl localhost:8080/support/alice/signal --json '{"id": "<the id>", "value": "approved"}'
 *
 * or the oldest wait for an approval: `{"name": "approval", "value": "approved"}`.
 */
import * as restate from '@restatedev/restate-sdk';
import { call, serve, signal, workflow } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

// A plain Restate service.
const orders = restate.service({
  name: 'orders',
  handlers: {
    status: async (_ctx: restate.Context, { order }: { order: string }) => `Order ${order} was delivered yesterday.`,
  },
});

// A workflow: a refund runs to the end, once, however long it takes.
const refund = restate.workflow({
  name: 'refund',
  handlers: {
    run: async (ctx: restate.WorkflowContext, { order, amount }: { order: string; amount: number }) => {
      await ctx.sleep({ seconds: 5 }); // a stand-in for the payment provider
      return `Refunded $${amount} for order ${order}.`;
    },
  },
});

const support = pi.agent({
  name: 'support',
  tools: [
    call(orders, 'status', {
      description: 'The status of an order.',
      input: z.object({ order: z.string() }),
    }),
    signal('approval', {
      description: "Wait for a manager to decide on a refund. Answers with the manager's decision.",
      input: z.object({ order: z.string(), amount: z.number() }),
    }),
    workflow(refund, {
      description: 'Refund an order. Only once a manager approved it.',
      input: z.object({ order: z.string(), amount: z.number() }),
    }),
  ],
});

await serve(orders, refund, support);

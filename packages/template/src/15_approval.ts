/**
 * 15 · A person in the loop
 *
 * A `signal` tool waits for a value from outside, such as a person's
 * approval, for as long as it takes: no process is held while it waits, and
 * a crash or a restart changes nothing. `pending` lists what the agent waits
 * for, and `signal` answers it.
 *
 *   node src/15_approval.ts                      # and register it (see 01)
 *   curl localhost:8080/support/alice/chat/send -d 'Order 103 arrived broken. Can I get my $30 back?'
 *   curl localhost:8080/support/alice/pending    # the approval it waits for
 *   curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'
 *   curl localhost:8080/support/alice/messages   # the refund, and the answer
 *
 * `/send` sends the message without waiting for the answer, which waits for the approval.
 */
import { serve, signal, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const refund = tool({
  name: 'refund',
  description: 'Refund an order. Only once a manager approved it.',
  input: z.object({ order: z.string(), amount: z.number() }),
  run: ({ order, amount }) => `Refunded $${amount} for order ${order}.`, // a stand-in for a payment provider
});

const support = pi.agent({
  name: 'support',
  tools: [
    signal('approval', {
      description: "Wait for a manager to decide on a refund. Answers with the manager's decision.",
      input: z.object({ order: z.string(), amount: z.number() }),
    }),
    refund,
  ],
});

await serve(support);

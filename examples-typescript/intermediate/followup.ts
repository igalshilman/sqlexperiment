/**
 * An agent that follows up later: it schedules a check-in with the customer a
 * day from now, a one-way call Restate keeps until then. Nothing runs in the
 * meantime, and a restart changes nothing.
 *
 *   ANTHROPIC_API_KEY=… node intermediate/followup.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/concierge/alice/chat -d 'My order 103 is late. Can you keep an eye on it?'
 *
 * The check-in shows in Restate's UI (localhost:9070) as a scheduled
 * invocation of `checkins/send`; FOLLOW_UP_SECONDS=10 makes it come sooner.
 */
import * as restate from '@restatedev/restate-sdk';
import { send, serve } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import { z } from 'zod';

// A stand-in for an email or a message to the customer.
const checkins = restate.service({
  name: 'checkins',
  handlers: {
    send: async (_ctx: restate.Context, { customer, note }: { customer: string; note: string }) => {
      console.log(`Checking in with ${customer}: ${note}`);
    },
  },
});

const concierge = claude.agent({
  name: 'concierge',
  system: 'You are a concierge for an online shop. When a customer needs it, schedule a check-in.',
  tools: [
    send(checkins, 'send', {
      name: 'schedule_checkin',
      description: 'Check in with the customer a day from now, with a note of what to check.',
      input: z.object({ customer: z.string(), note: z.string() }),
      delay: { seconds: Number(process.env.FOLLOW_UP_SECONDS ?? 24 * 60 * 60) },
    }),
  ],
});

await serve(checkins, concierge);

/**
 * A front desk that routes each message to the team that handles it: a
 * handler of your own, where your code decides, and the agents answer.
 *
 * The desk asks a model which team it is (`pi.prompt`, answered once), then
 * calls that team's agent like any Restate handler. Each team keeps its own
 * conversation with each user (`billing/alice`, `tech/alice`).
 *
 *   OPENAI_API_KEY=… node intermediate/router.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/desk/alice/chat -d 'I was charged twice for order 103'
 *   curl localhost:8080/desk/alice/chat -d 'The app crashes when I open my orders'
 *   curl localhost:8080/billing/alice/messages   # what billing was asked, and answered
 */
import { chatHandler, sessionObject, serve } from 'restate-agents';
import * as langgraph from 'restate-agents/langgraph';
import * as pi from 'restate-agents/pi';

const billing = langgraph.agent({
  name: 'billing',
  system: 'You handle billing: charges, refunds and invoices. Be brief and kind.',
});

const tech = pi.agent({
  name: 'tech',
  system: 'You handle technical problems with the app. Ask for what you need to know, one question at a time.',
});

const desk = sessionObject({
  name: 'desk',
  handlers: {
    chat: chatHandler(async (ctx, message) => {
      // Asked once: a retried attempt gets the same answer.
      const team = await pi.prompt(
        ctx,
        `Which team handles this message, billing or tech? Answer with the one word.\n\n${message}`,
      );
      const agent = /billing/i.test(team) ? billing : tech;
      // This handler is not replayed: the idempotency key makes a retried
      // attempt reach the call the first one made, not send the message again.
      return ctx.rpc(agent, ctx.key, { idempotencyKey: `${ctx.request.id}:${agent.name}` }).chat(message);
    }),
  },
});

await serve(desk, billing, tech);

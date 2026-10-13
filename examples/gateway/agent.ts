// A support agent on pi-durable whose tools are the gateway's: an Assistant
// object per user, keyed `<tenant>.<user>` (`acme.alice`), whose pi-durable
// storage is the object's database (../pi/with-pi.ts).
//
// Each attempt asks the gateway for the tenant's tools, and a tool call is
// one Gateway.call, with an idempotency key made from the request id (the
// same on every attempt) and the tool call id. The agent has no journal: a
// retried attempt reopens pi-durable from the database, runs an unfinished
// tool call again with the same call id, and so reaches the gateway call
// the first attempt made.
//
// Environment: PORT, PI_MODEL (the scripted `concierge/concierge` below by
// default, or a real one, such as anthropic/claude-sonnet-4-5) and
// ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { Type, type TranscriptContext } from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import { defineTool } from '@earendil-works/pi-durable';
import { serveIfMain } from '../faults.ts';
import { callTools, fakeProvider, lastText, say, textOf } from '../pi/utils.ts';
import { withPi } from '../pi/with-pi.ts';
import { gateway, identity } from './gateway.ts';

/**
 * The scripted model: "where are orders 101 to 108?" looks them all up at
 * once, "refund 30 of order 103" refunds, and it reports what the tools
 * answered.
 */
export function concierge(context: TranscriptContext) {
  const { messages } = context;
  const results = [];
  for (let i = messages.length - 1; messages[i]?.role === 'toolResult'; i--) results.unshift(messages[i]!);
  if (results.length > 0) return say(results.map((r) => textOf(r.content)).join('\n'));
  const text = lastText(context);
  const refund = /refund (\d+) of order (\d+)/i.exec(text);
  if (refund) return callTools([['refund', { order: refund[2]!, amount: Number(refund[1]) }]]);
  const range = /orders? (\d+)(?: to (\d+))?/i.exec(text);
  if (range) {
    const first = Number(range[1]);
    const ids = Array.from({ length: Number(range[2] ?? first) - first + 1 }, (_, i) => String(first + i));
    return callTools(ids.map((order) => ['lookup_order', { order }]));
  }
  return say('Ask me "where are orders 101 to 108?" or "refund 30 of order 103".');
}

const models = createModels();
const model = process.env.PI_MODEL ?? 'concierge/concierge';
const [provider] = model.split('/');
if (provider === 'concierge') {
  models.setProvider(fakeProvider('concierge', concierge));
} else {
  const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
  models.setProvider(module[`${provider}Provider`]());
}

export const assistant = restate.object({
  name: 'Assistant',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: async (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) => {
      const { tenant, user } = identity(ctx.key);
      const gw = ctx.serviceClient(gateway);
      const tools = (await gw.tools({ tenant })).map((tool) =>
        defineTool({
          name: tool.name,
          description: tool.description,
          parameters: Type.Unsafe<Record<string, unknown>>(tool.inputSchema),
          replay: 'safe', // runs again after a crash, with the same call id
          execute: async (args, api) => {
            const { content, isError = false } = await gw.call(
              { tenant, user, name: tool.name, arguments: args },
              restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${api.callId}` }),
            );
            return { content, isError };
          },
        }),
      );
      return withPi(ctx, { models, model, tools }, async (pi) => ({ answer: await pi.prompt(id, text) }));
    },
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [assistant]);

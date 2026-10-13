// A support agent on pi-durable, whose tools are the gateway's: an Assistant
// object per tenant and user (key `<tenant>.<user>`, such as `acme.alice`),
// whose pi-durable storage is the object's database (../pi/with-pi.ts).
//
// It has no tools of its own. Each attempt asks the gateway for the tenant's
// tools, and offers them to the model as they are listed: name, description
// and input schema. A tool call is one call to the gateway, `Gateway.call`,
// with an idempotency key made from the request id (the same on every
// attempt) and the tool call id. The agent has no journal: a retried attempt
// reopens pi-durable from the database, runs an unfinished tool call again
// with the same call id, and so reaches the gateway call the first attempt
// made, which carries on or has its result. However often the agent crashes,
// each tool call is approved once, runs once, and is audited once.
//
// Environment: PORT, PI_MODEL (`concierge/concierge`, the scripted model
// below, by default; or a real one, such as anthropic/claude-sonnet-4-5) and
// ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { Type, type TranscriptContext } from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import { defineTool } from '@earendil-works/pi-durable';
import { serveIfMain } from '../faults.ts';
import { callTools, fakeProvider, lastText, say, textOf } from '../pi/utils.ts';
import { withPi } from '../pi/with-pi.ts';
import { gateway } from './gateway.ts';

/**
 * The scripted concierge: "where are orders 101 to 108?" looks them all up at
 * once, "refund 30 of order 103 because …" refunds, "email the customer of
 * order 103: …" mails; and it reports what the tools answered.
 */
export function concierge(context: TranscriptContext) {
  const messages = context.messages;
  if (messages.at(-1)!.role === 'toolResult') {
    // The results of the last round of tool calls.
    const results = [];
    for (let i = messages.length - 1; i >= 0 && messages[i]!.role === 'toolResult'; i--) results.unshift(messages[i]!);
    return say(
      results.map((r) => `${r.role === 'toolResult' && r.isError ? 'failed: ' : ''}${textOf(r.content)}`).join('\n'),
    );
  }
  const text = lastText(context);
  const range = /orders? (\d+)(?: (?:to|-) (\d+))?/i.exec(text);
  const refund = /refund (\d+) of order (\d+)(?: because (.*))?/i.exec(text);
  const email = /email the customer of order (\d+):\s*(.*)/i.exec(text);
  if (refund) {
    const [, amount, order = '', reason = 'a broken item'] = refund;
    return callTools([['refund', { order, amount: Number(amount), reason }]], 'Asking for the refund.');
  }
  if (email) return callTools([['email_customer', { order: email[1]!, text: email[2]! }]]);
  if (range) {
    const first = Number(range[1]);
    const last = Number(range[2] ?? range[1]);
    const ids = Array.from({ length: last - first + 1 }, (_, i) => String(first + i));
    return callTools(
      ids.map((order) => ['lookup_order', { order }]),
      `Looking up ${ids.length} order${ids.length > 1 ? 's' : ''}.`,
    );
  }
  return say(
    'Ask me "where are orders 101 to 108?", "refund 30 of order 103", or "email the customer of order 103: …".',
  );
}

async function models() {
  const providers = createModels();
  const model = process.env.PI_MODEL ?? 'concierge/concierge';
  const [provider] = model.split('/');
  if (provider === 'concierge') {
    providers.setProvider(fakeProvider('concierge', concierge));
  } else {
    const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
    providers.setProvider(module[`${provider}Provider`]());
  }
  return { models: providers, model };
}

const { models: registry, model } = await models();

/** The tenant's tools, as the gateway lists them, each a call to the gateway. */
async function gatewayTools(ctx: restate.SqlObjectContext, tenant: string) {
  const gw = ctx.serviceClient(gateway);
  const tools = await gw.tools({ tenant });
  return tools.map((tool) =>
    defineTool({
      name: tool.name,
      description: tool.description,
      parameters: Type.Unsafe<Record<string, unknown>>(tool.inputSchema),
      replay: 'safe', // runs again after a crash, with the same call id
      execute: async (args, api) => {
        const result = await gw.call(
          { tenant, caller: ctx.key, name: tool.name, arguments: args },
          restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${api.callId}` }),
        );
        return { content: result.content, isError: result.isError ?? false };
      },
    }),
  );
}

export const assistant = restate.object({
  name: 'Assistant',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: async (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) => {
      const tenant = ctx.key.split('.')[0]!;
      const tools = await gatewayTools(ctx, tenant);
      return withPi(ctx, { models: registry, model, tools }, async (pi) => ({ answer: await pi.prompt(id, text) }));
    },
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [assistant]);

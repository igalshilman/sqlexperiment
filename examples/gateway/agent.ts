// A support agent on pi-durable whose tools are the gateway's: an Assistant
// object per user, keyed `<tenant>.<user>` (`acme.alice`), whose pi-durable
// storage is the object's database (../pi/with-pi.ts).
//
// Each attempt asks the tenant's gateway for its tools, and a tool call is
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
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { callTools, fakeProvider, lastText, say, textOf } from '../pi/utils.ts';
import { withPi } from '../pi/with-pi.ts';
import { gateway, identity } from './gateway.ts';

/** A tool's result, in a sentence: an order, a refund or a ticket, or else the text as it is. */
export function sentence(text: string): string {
  try {
    const r = JSON.parse(text) as Partial<Record<'id' | 'status' | 'refund' | 'ticket', string> & { total: number }>;
    if (r.status && r.total !== undefined) return `Order ${r.id} is ${r.status}, $${r.total}.`;
    if (r.refund) return `Refund ${r.refund} is made.`;
    if (r.ticket) return `Ticket ${r.ticket} is open.`;
  } catch {
    // not JSON
  }
  return text;
}

/**
 * The scripted model: "where are orders 101 to 108?" looks them all up at
 * once, "refund 30 of order 103" refunds, "open a ticket for order 104: …"
 * opens one, and it reports what the tools
 * answered, a sentence each.
 */
export function concierge(context: TranscriptContext) {
  const { messages } = context;
  const results = [];
  for (let i = messages.length - 1; messages[i]?.role === 'toolResult'; i--) results.unshift(messages[i]!);
  if (results.length > 0) return say(results.map((r) => sentence(textOf(r.content))).join('\n'));
  const text = lastText(context);
  const refund = /refund (\d+) of order (\d+)/i.exec(text);
  if (refund) return callTools([['refund', { order: refund[2]!, amount: Number(refund[1]) }]]);
  const ticket = /ticket for order (\d+):?\s*(.*)/i.exec(text);
  if (ticket) return callTools([['open_ticket', { order: ticket[1]!, summary: ticket[2] || 'no summary' }]]);
  const range = /orders? (\d+)(?: to (\d+))?/i.exec(text);
  if (range) {
    const first = Number(range[1]);
    const ids = Array.from({ length: Number(range[2] ?? first) - first + 1 }, (_, i) => String(first + i));
    return callTools(ids.map((order) => ['lookup_order', { order }]));
  }
  return say(`Ask me, for example: ${EXAMPLES.map((e) => `"${e}"`).join(', ')}`);
}

/** Prompts to try, which the scripted model understands too. */
export const EXAMPLES = [
  'Where are orders 101 to 105, and what did they cost?',
  'Where is order 201?',
  'Refund 30 of order 103, it arrived damaged.',
  'Open a ticket for order 104: the box was empty.',
  'Where are orders 101 to 112?',
];

// The suggestion is a JSON schema `default` only: a zod `.default()` would
// accept an empty body, and the SDK would then announce no input at all.
export const PROMPT_SCHEMA = z
  .string()
  .describe('What the customer asks')
  .meta({ default: EXAMPLES[0], examples: EXAMPLES });
const Answer = z.object({ answer: z.string().describe("The assistant's answer") });

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
    prompt: restate.handlers.object.exclusive(
      {
        description: [
          'Answer a prompt, with the tools of the tenant of the key `<tenant>.<user>` (such as `acme.alice`).',
          'Prompts to try:',
          ...EXAMPLES.map((e) => `- ${e}`),
          'A refund waits for a manager: `Gateway/<tenant>/pending`, then `Gateway/<tenant>/decide`.',
        ].join('\n'),
        metadata: Object.fromEntries(EXAMPLES.map((e, i) => [`example.${i + 1}`, e])),
        input: restate.serde.schema(PROMPT_SCHEMA),
        output: restate.serde.schema(Answer),
        journalReplay: false,
      },
      async (ctx: restate.SqlObjectContext, text: z.infer<typeof PROMPT_SCHEMA>) => {
        const { tenant, user } = identity(ctx.key);
        const gw = ctx.objectClient(gateway, tenant);
        const tools = (await gw.tools()).map((tool) =>
          defineTool({
            name: tool.name,
            description: tool.description,
            parameters: Type.Unsafe<Record<string, unknown>>(tool.inputSchema),
            replay: 'safe', // runs again after a crash, with the same call id
            execute: async (args, api) => {
              const { content, isError = false } = await gw.call(
                { user, name: tool.name, arguments: args },
                restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${api.callId}` }),
              );
              return { content, isError };
            },
          }),
        );
        // Submitted once: a retried attempt resumes the prompt of the same invocation id.
        return withPi(ctx, { models, model, tools }, async (pi) => ({
          answer: await pi.prompt(ctx.request().id, text),
        }));
      },
    ),
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [assistant]);

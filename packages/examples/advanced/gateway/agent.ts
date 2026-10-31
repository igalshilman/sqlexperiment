// A support agent on pi-durable whose tools are the gateway's: an Assistant
// object per user, keyed `<tenant>.<user>` (`acme.alice`), whose pi-durable
// storage is the object's database (restate-agents/pi).
//
// Each attempt asks the tenant's gateway for its tools, and a tool call is
// one Gateway.call, with an idempotency key made from the request id (the
// same on every attempt) and the tool call id. The agent has no journal: a
// retried attempt reopens pi-durable from the database, runs an unfinished
// tool call again with the same call id, and so reaches the gateway call
// the first attempt made.
//
// Sub-agents: the `investigate` tool hands an order to its case agent:
// a Case object per order, keyed `<tenant>.<order>` (`acme.103`), a pi agent
// of its own, with its own database and the gateway's tools. Each order is
// another object: they run at once, and one holds no other up. The same
// order is the same object for every user, and remembers each report on it.
//
// Files: with WORKSPACE set, each object gets a directory of its own,
// `$WORKSPACE/<service>/<key>`, and pi-durable's coding tools over it
// (read, write, edit, bash; see restate-agents/pi). Run it in a sandbox:
// `bash` runs any command, within the directory or outside it.
//
// Environment: PORT, MODEL (see restate-agents/pi) with its provider's
// API key, and WORKSPACE.
import * as restate from 'restate-objects';
import { Type } from '@earendil-works/pi-ai';
import { defineTool, type ToolRegistration } from '@earendil-works/pi-durable';
import { z } from 'zod';
import { object } from 'restate-agents';
import { withPi } from 'restate-agents/pi';
import { join } from 'node:path';
import { gateway, identity } from './gateway.ts';

/**
 * The object's own directory in WORKSPACE, if set: `<service>/<key>`, the
 * key's characters other than letters, digits, `.`, `_` and `-` as `_`.
 */
export function workspaceOf(service: string, key: string, root = process.env.WORKSPACE): string | undefined {
  if (!root) return undefined;
  const name = key.replace(/[^\w.-]/g, '_');
  return join(root, service, /^\.*$/.test(name) ? `_${name}` : name);
}

/** Prompts to try. */
export const EXAMPLES = [
  'Where are orders 101 to 105, and what did they cost?',
  'Where is order 201?',
  'Refund 30 of order 103, it arrived damaged.',
  'Open a ticket for order 104: the box was empty.',
  'Where are orders 101 to 112?',
  'Investigate orders 103 and 104: the boxes arrived crushed.',
  // With a workspace only.
  ...(process.env.WORKSPACE
    ? ['Write notes.md: order 103 arrived crushed, refund pending.', 'Read notes.md', 'Run: ls -la && cat notes.md']
    : []),
];

// The suggestion is a JSON schema `default` only: a zod `.default()` would
// accept an empty body, and the SDK would then announce no input at all.
export const PROMPT_SCHEMA = z
  .string()
  .describe('What the customer asks')
  .meta({ default: EXAMPLES[0], examples: EXAMPLES });
const Answer = z.object({
  answer: z.string().describe("The assistant's answer"),
});

const CaseReport = z.object({
  user: z.string().describe('Who reports'),
  question: z.string().describe('What happened to the order'),
});

/**
 * The tenant's gateway tools, for `user`. A call is one Gateway.call, with
 * an idempotency key made from the request id (the same on every attempt)
 * and the tool call id.
 */
async function gatewayTools(ctx: restate.ObjectContext, tenant: string, user: string) {
  const gw = ctx.objectClient(gateway, tenant);
  return (await gw.tools()).map((tool): ToolRegistration =>
    defineTool({
      name: tool.name,
      description: tool.description,
      parameters: Type.Unsafe<Record<string, unknown>>(tool.inputSchema),
      replay: 'safe', // runs again after a crash, with the same call id
      execute: async (args, api) => {
        const { content, isError = false } = await gw.call(
          { user, name: tool.name, arguments: args },
          restate.rpc.opts({
            idempotencyKey: `${ctx.request().id}:${api.callId}`,
          }),
        );
        return { content, isError };
      },
    }),
  );
}

// Journals kept an hour, to look at in the UI (by default none are kept).
const options = { journalRetention: { hours: 1 } };

export const caseAgent = object({
  name: 'Case',
  handlers: {
    report: restate.handlers.object.exclusive(
      {
        description: [
          "Look into a report on the order of the key `<tenant>.<order>` (such as `acme.103`), with the tenant's tools.",
          'The case remembers every report on it, from any user.',
        ].join('\n'),
        input: restate.serde.schema(CaseReport),
        output: restate.serde.schema(Answer),
      },
      async (ctx: restate.ObjectContext, { user, question }: z.infer<typeof CaseReport>) => {
        const [tenant = '', order = ''] = ctx.key.split('.', 2);
        const tools = await gatewayTools(ctx, tenant, user);
        const workspace = workspaceOf('Case', ctx.key);
        return withPi(ctx, { tools, workspace }, async (session) => ({
          answer: await session.prompt(`Order ${order}, from ${user}: ${question}`),
        }));
      },
    ),
  },
  options,
});

/** Answer prompt `text`, with the tenant's tools and case agents. */
export const prompt = async (ctx: restate.ObjectContext, text: z.infer<typeof PROMPT_SCHEMA>) => {
  const { tenant, user } = identity(ctx.key);
  const tools = await gatewayTools(ctx, tenant, user);
  tools.push(
    defineTool({
      name: 'investigate',
      description:
        "Hand an order to its case agent, which looks into it and remembers every report on it; answer with the case agent's answer. One call per order: they run at once.",
      parameters: Type.Object({
        order: Type.String({
          description: 'The order id, such as "103"',
        }),
        question: Type.String({
          description: "What happened to the order, in the customer's words",
        }),
      }),
      replay: 'safe',
      execute: async ({ order, question }, api) => {
        const { answer } = await ctx.objectClient(caseAgent, `${tenant}.${order}`).report(
          { user, question },
          restate.rpc.opts({
            idempotencyKey: `${ctx.request().id}:${api.callId}`,
          }),
        );
        return { output: [{ type: 'text', text: answer }] };
      },
    }),
  );
  // Submitted once: a retried attempt resumes the prompt of the same invocation id.
  const workspace = workspaceOf('Assistant', ctx.key);
  return withPi(ctx, { tools, workspace }, async (session) => ({
    answer: await session.prompt(text),
  }));
};

export const assistant = object({
  name: 'Assistant',
  handlers: {
    prompt: restate.handlers.object.exclusive(
      {
        description: [
          'Answer a prompt, with the tools of the tenant of the key `<tenant>.<user>` (such as `acme.alice`).',
          'Prompts to try:',
          ...EXAMPLES.map((e) => `- ${e}`),
          'A refund waits for a manager: `Gateway/<tenant>/pending`, then `Gateway/<tenant>/decide`.',
          'Investigating an order hands it to its case agent, `Case/<tenant>.<order>`.',
        ].join('\n'),
        metadata: Object.fromEntries(EXAMPLES.map((e, i) => [`example.${i + 1}`, e])),
        input: restate.serde.schema(PROMPT_SCHEMA),
        output: restate.serde.schema(Answer),
      },
      prompt,
    ),
  },
  options,
});

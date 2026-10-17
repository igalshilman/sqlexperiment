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
// Sub-agents: the `investigate` tool hands an order to its case agent:
// a Case object per order, keyed `<tenant>.<order>` (`acme.103`), a pi agent
// of its own, with its own database and the gateway's tools. Each order is
// another object: they run at once, and one holds no other up. The same
// order is the same object for every user, and remembers each report on it.
//
// Files: with WORKSPACE set, each object gets a directory of its own,
// `$WORKSPACE/<service>/<key>`, and pi-durable's coding tools over it
// (read, write, edit, bash; see ../pi/with-pi.ts). Run it in a sandbox:
// `bash` runs any command, within the directory or outside it.
//
// Environment: PORT, PI_MODEL (the scripted `concierge/concierge` and
// `caseworker/caseworker` below by default, or a real one for both, such as
// anthropic/claude-sonnet-4-5), WORKSPACE and ABORT_PER_MILLE (see
// ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { Type, type TranscriptContext } from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import { defineTool, type ToolRegistration } from '@earendil-works/pi-durable';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { callTools, fakeProvider, lastText, type Respond, say, textOf } from '../pi/utils.ts';
import { withPi } from '../pi/with-pi.ts';
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

/** The results of the last round of tool calls, a sentence each. */
function lastResults({ messages }: TranscriptContext) {
  const results = [];
  for (let i = messages.length - 1; messages[i]?.role === 'toolResult'; i--) results.unshift(messages[i]!);
  return results.map((r) => sentence(textOf(r.content)));
}

/**
 * The scripted model: "write notes.md: …", "read notes.md" and
 * "run: ls -l" use the workspace's files, "investigate orders 103 and 104: …" hands each order
 * to its case agent at once, "where are orders 101 to 108?" looks them all
 * up at once, "refund 30 of order 103" refunds, "open a ticket for order
 * 104: …" opens one, and it reports what the tools answered, a sentence each.
 */
export function concierge(context: TranscriptContext) {
  const results = lastResults(context);
  if (results.length > 0) return say(results.join('\n'));
  const text = lastText(context);
  const write = /^write ([^\s:]+):\s*([^]*)$/i.exec(text);
  if (write) return callTools([['write', { path: write[1]!, content: write[2]! }]]);
  const read = /^read (\S+)$/i.exec(text.trim());
  if (read) return callTools([['read', { path: read[1]! }]]);
  const run = /^run:?\s+([^]+)$/i.exec(text);
  if (run) return callTools([['bash', { command: run[1]! }]]);
  if (/^\s*investigate\b/i.test(text)) {
    const [orders = '', question = text] = text.split(/:\s*/, 2);
    return callTools((orders.match(/\d+/g) ?? []).map((order) => ['investigate', { order, question }]));
  }
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

/**
 * The scripted case agent: a report on its order looks the order up and
 * opens a ticket, and its answer says what came of them and what was
 * reported on the case before, by whom.
 */
export function caseworker(context: TranscriptContext) {
  const reports = context.messages.flatMap((m) => {
    const report = m.role === 'user' ? /^Order (\S+), from (\S+): ([^]*)$/.exec(textOf(m.content)) : null;
    return report ? [{ order: report[1]!, user: report[2]!, text: report[3]! }] : [];
  });
  const report = reports.at(-1);
  if (!report) return say('Report on an order as "Order <id>, from <user>: <what happened>".');
  const results = lastResults(context);
  if (results.length === 0)
    return callTools([
      ['lookup_order', { order: report.order }],
      ['open_ticket', { order: report.order, summary: `${report.user}: ${report.text}` }],
    ]);
  const before = reports.slice(0, -1).map((r) => `${r.user}: "${r.text}"`);
  const history = before.length > 0 ? `Reported before: ${before.join('; ')}.` : 'The first report on it.';
  return say(`Case ${report.order}: ${results.join(' ')} ${history}`);
}

/** Prompts to try, which the scripted model understands too. */
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

/** pi-ai models, with `name` the scripted model, unless PI_MODEL names a real one. */
async function modelOr(name: string, respond: Respond) {
  const models = createModels();
  const model = process.env.PI_MODEL || `${name}/${name}`;
  const [provider] = model.split('/');
  if (provider === name) {
    models.setProvider(fakeProvider(name, respond));
  } else {
    const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
    models.setProvider(module[`${provider}Provider`]());
  }
  return { models, model };
}
const assistantModel = await modelOr('concierge', concierge);
const caseModel = await modelOr('caseworker', caseworker);

/**
 * The tenant's gateway tools, for `user`. A call is one Gateway.call, with
 * an idempotency key made from the request id (the same on every attempt)
 * and the tool call id.
 */
async function gatewayTools(ctx: restate.SqlObjectContext, tenant: string, user: string) {
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

// Hide the SQL storage from the UI's journal view: the database's pages,
// log and header in the object's state (`v13/...`), the sends pending with
// a transaction, and the `sync` run before an attempt ends.
const metadata = {
  'dev.restate.tooling.journal.hidden.names': 'sync,_restate_sql_sends',
  'dev.restate.tooling.journal.hidden.name.prefix': 'v13/',
};
const options = {
  ...restate.sqlOptions,
  // A prompt is long work, and each attempt resumes from storage.
  retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  // Kept an hour, to look at in the UI (sqlOptions keeps none).
  journalRetention: { hours: 1 },
};

export const caseAgent = restate.object({
  name: 'Case',
  handlers: {
    report: restate.createObjectHandler(
      {
        description: [
          "Look into a report on the order of the key `<tenant>.<order>` (such as `acme.103`), with the tenant's tools.",
          'The case remembers every report on it, from any user.',
        ].join('\n'),
        input: restate.serde.schema(CaseReport),
        output: restate.serde.schema(Answer),
        journalReplay: false,
      },
      async (ctx: restate.SqlObjectContext, { user, question }: z.infer<typeof CaseReport>) => {
        const [tenant = '', order = ''] = ctx.key.split('.', 2);
        const tools = await gatewayTools(ctx, tenant, user);
        const workspace = workspaceOf('Case', ctx.key);
        return withPi(ctx, { ...caseModel, tools, workspace }, async (pi) => ({
          answer: await pi.prompt(ctx.request().id, `Order ${order}, from ${user}: ${question}`),
        }));
      },
    ),
  },
  metadata,
  options,
});

/** Answer prompt `text`, with the tenant's tools and case agents. */
export const prompt = async (ctx: restate.SqlObjectContext, text: z.infer<typeof PROMPT_SCHEMA>) => {
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
        return {
          content: [{ type: 'text', text: answer }],
          isError: false,
        };
      },
    }),
  );
  // Submitted once: a retried attempt resumes the prompt of the same invocation id.
  const workspace = workspaceOf('Assistant', ctx.key);
  return withPi(ctx, { ...assistantModel, tools, workspace }, async (pi) => ({
    answer: await pi.prompt(ctx.request().id, text),
  }));
};

export const assistant = restate.object({
  name: 'Assistant',
  handlers: {
    prompt: restate.createObjectHandler(
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
        journalReplay: false,
      },
      prompt,
    ),
  },
  metadata,
  options,
});

serveIfMain(import.meta, [assistant, caseAgent]);

// A Claude agent in a Restate virtual object, on the tool runner of the
// Anthropic SDK (`client.beta.messages.toolRunner`): a ClaudeAgent object per
// conversation, whose messages are in the object's database
// (conversation.ts). Its tools move money in the ledger (../node/ledger.ts).
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// runs again from the start, and starts the runner with the stored messages:
// - Each reply of the model is stored before its tools run, and their results
//   before the next request. So a crash during the tools resumes them with the
//   same tool call ids, without asking the model again.
// - A tool uses its tool call id as the transfer id, so a tool run again moves
//   the money once; and its call to the ledger has an idempotency key made
//   from the request id (`ctx.request().id`, the same on every attempt) and
//   the tool call id, so a retried attempt reaches the call the first one made.
// - A tool's error fails the attempt, which Restate retries (by default the
//   runner hands it to the model as the tool's result).
// - A prompt is answered once per prompt id: its answer is stored with the
//   model's last reply, and a prompt asked again returns it.
//
// Environment: PORT (default 9080), ABORT_PER_MILLE (see ../faults.ts), and
// CLAUDE_MODEL (see teller.ts).
import * as restate from '@restatedev/restate-sdk';
import type Anthropic from '@anthropic-ai/sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import type { BetaRunnableTool, BetaToolRunContext } from '@anthropic-ai/sdk/lib/tools/BetaRunnableTool';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../node/ledger.ts';
import { Conversation } from './conversation.ts';
import { clientFromEnv } from './teller.ts';

/** The id of the tool call a tool runs for: the runner always passes it. */
const callId = (context?: BetaToolRunContext) => {
  if (!context) throw new Error('a tool call without its context');
  return context.toolUse.id;
};

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx: restate.SqlObjectContext): BetaRunnableTool[] => {
  const books = ctx.objectClient(ledger, 'bank');
  // One idempotency key per tool call: a prompt makes several.
  const key = (context?: BetaToolRunContext) => `${ctx.request().id}:${callId(context)}`;
  return [
    betaZodTool({
      name: 'transfer',
      description: 'Move an amount of money from one account to another.',
      inputSchema: z.object({
        from: z.number().int().describe('Account to take the money from'),
        to: z.number().int().describe('Account to put the money in'),
        amount: z.number().int().min(1),
      }),
      run: async ({ from, to, amount }, context) => {
        const id = `ClaudeAgent/${ctx.key}/${callId(context)}`; // unique in the ledger
        const applied = await books.transfer(
          { id, from, to, amount },
          restate.rpc.opts({ idempotencyKey: key(context) }),
        );
        return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
      },
    }),
    betaZodTool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      inputSchema: z.object({}),
      run: async (_args, context) =>
        JSON.stringify(await books.audit(restate.rpc.opts({ idempotencyKey: key(context) }))),
    }),
  ];
};

/**
 * `tools`, whose errors fail the attempt: the runner would hand a thrown error
 * to the model as the tool's result, so each one is kept, for `check()` to
 * throw once the tools ran.
 */
function failing(tools: BetaRunnableTool[]) {
  let failure: unknown;
  return {
    tools: tools.map((tool): BetaRunnableTool => ({
      ...tool,
      run: async (args, context) => {
        try {
          return await tool.run(args, context);
        } catch (e) {
          failure ??= e;
          throw e;
        }
      },
    })),
    check: () => {
      if (failure !== undefined) throw failure;
    },
  };
}

const textOf = (content: Anthropic.Beta.Messages.BetaContentBlock[]) =>
  content
    .filter((c) => c.type === 'text')
    .map((c) => c.text)
    .join('');

const { client, model } = clientFromEnv();

/** Run prompt `id` of `conversation` until it is answered, from its stored messages. */
async function run(ctx: restate.SqlObjectContext, conversation: Conversation, id: string) {
  const { tools, check } = failing(bank(ctx));
  const runner = client.beta.messages.toolRunner({
    model,
    max_tokens: 1024,
    system: 'You keep the books of a bank, with your tools. Answer briefly.',
    tools,
    messages: await conversation.messages(),
  });
  // A reply whose tools did not finish (a crash): run them, with the same ids.
  if (runner.params.messages.at(-1)?.role === 'assistant') {
    const results = await runner.generateToolResponse();
    check();
    if (results) {
      await conversation.append(id, [results]);
      runner.pushMessages(results);
    }
  }
  for await (const message of runner) {
    const reply: Anthropic.Beta.Messages.BetaMessageParam = { role: 'assistant', content: message.content };
    if (!message.content.some((c) => c.type === 'tool_use')) {
      await conversation.append(id, [reply], textOf(message.content));
      return;
    }
    // The reply first, then its tools.
    await conversation.append(id, [reply]);
    const results = await runner.generateToolResponse();
    check();
    if (results) await conversation.append(id, [results]);
  }
}

/** Answer prompt `text`, once per prompt `id`. */
export const prompt = (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) =>
  ctx.sqlSession(async (db) => {
    const conversation = await Conversation.open(db);
    for (;;) {
      const answer = await conversation.answer(id);
      if (answer !== undefined) return { answer };
      // Finish an interrupted prompt first (this one's, after a crash),
      // then start this one.
      let pending = await conversation.pending();
      if (pending === undefined) {
        await conversation.append(id, [{ role: 'user', content: text }]);
        pending = id;
      }
      await run(ctx, conversation, pending);
    }
  });

export const agent = restate.object({
  name: 'ClaudeAgent',
  handlers: { prompt },
  options: {
    ...restate.sqlOptions,
    // Keep retrying rather than pause after Restate's default number of
    // attempts: each attempt resumes from the stored messages.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

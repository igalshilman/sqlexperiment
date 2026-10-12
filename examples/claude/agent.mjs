// A Claude agent in a Restate virtual object, on the tool runner of the
// Anthropic SDK (`client.beta.messages.toolRunner`): a ClaudeAgent object per
// conversation, whose messages are in the object's database
// (conversation.mjs). Its tools move money in the ledger (../node/ledger.mjs).
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
// Environment: PORT (default 9080), ABORT_PER_MILLE (see ../faults.mjs), and
// CLAUDE_MODEL (see teller.mjs).
import * as restate from '@restatedev/restate-sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import { z } from 'zod';
import { serveIfMain } from '../faults.mjs';
import { ledger } from '../node/ledger.mjs';
import { Conversation } from './conversation.mjs';
import { clientFromEnv } from './teller.mjs';

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx) => {
  const books = ctx.objectClient(ledger, 'bank');
  // One idempotency key per tool call: a prompt makes several.
  const opts = (toolUse) => restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${toolUse.id}` });
  return [
    betaZodTool({
      name: 'transfer',
      description: 'Move an amount of money from one account to another.',
      inputSchema: z.object({
        from: z.number().int().describe('Account to take the money from'),
        to: z.number().int().describe('Account to put the money in'),
        amount: z.number().int().min(1),
      }),
      run: async ({ from, to, amount }, { toolUse }) => {
        const id = `ClaudeAgent/${ctx.key}/${toolUse.id}`; // unique in the ledger
        await books.transfer({ id, from, to, amount }, opts(toolUse));
        return `moved ${amount} from account ${from} to account ${to}`;
      },
    }),
    betaZodTool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      inputSchema: z.object({}),
      run: async (_args, { toolUse }) => JSON.stringify(await books.audit(opts(toolUse))),
    }),
  ];
};

/**
 * `tools`, whose errors fail the attempt: the runner would hand a thrown error
 * to the model as the tool's result, so each one is kept, for `check()` to
 * throw once the tools ran.
 */
function failing(tools) {
  let failure;
  return {
    tools: tools.map((tool) => ({
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
    check() {
      if (failure) throw failure;
    },
  };
}

const textOf = (content) =>
  content
    .filter((c) => c.type === 'text')
    .map((c) => c.text)
    .join('');

const { client, model } = clientFromEnv();

/** Run prompt `id` of `conversation` until it is answered, from its stored messages. */
async function run(ctx, conversation, id) {
  const { tools, check } = failing(bank(ctx));
  const runner = client.beta.messages.toolRunner({
    model,
    max_tokens: 1024,
    system: 'You keep the books of a bank, with your tools. Answer briefly.',
    tools,
    messages: await conversation.messages(),
  });
  // A reply whose tools did not finish (a crash): run them, with the same ids.
  if (runner.params.messages.at(-1).role === 'assistant') {
    const results = await runner.generateToolResponse();
    check();
    await conversation.append(id, [results]);
    runner.pushMessages(results);
  }
  for await (const message of runner) {
    const reply = { role: 'assistant', content: message.content };
    if (!message.content.some((c) => c.type === 'tool_use')) {
      await conversation.append(id, [reply], textOf(message.content));
      return;
    }
    // The reply first, then its tools.
    await conversation.append(id, [reply]);
    const results = await runner.generateToolResponse();
    check();
    await conversation.append(id, [results]);
  }
}

export const agent = restate.object({
  name: 'ClaudeAgent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: (ctx, { id, text }) =>
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
      }),
  },
  options: {
    ...restate.sqlOptions,
    // Keep retrying rather than pause after Restate's default number of
    // attempts: each attempt resumes from the stored messages.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

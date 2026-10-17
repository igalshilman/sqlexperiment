// The bank agent on Claude (restate-agents/claude): a ClaudeAgent object per
// conversation. Its tools move money in the ledger (../sql/ledger.ts), with
// the tool call id as the transfer id, so a tool run again after a crash moves
// the money once; and its call to the ledger has an idempotency key made from
// the request id and the tool call id, so a retried attempt reaches the call
// the first one made.
//
// Environment: PORT (default 9080), ANTHROPIC_API_KEY, CLAUDE_MODEL and
// ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import * as claude from 'restate-agents/claude';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../sql/ledger.ts';

/** The ledger. */
const books = (ctx: restate.SqlObjectContext) => ctx.objectClient(ledger, 'bank');
/** One idempotency key per tool call: a prompt makes several. */
const key = (ctx: restate.SqlObjectContext, id: string) => `${ctx.request().id}:${id}`;

const transfer = (ctx: restate.SqlObjectContext) =>
  betaZodTool({
    name: 'transfer',
    description: 'Move an amount of money from one account to another.',
    inputSchema: z.object({
      from: z.number().int().describe('Account to take the money from'),
      to: z.number().int().describe('Account to put the money in'),
      amount: z.number().int().min(1),
    }),
    run: async ({ from, to, amount }, context) => {
      const id = `ClaudeAgent/${ctx.key}/${claude.callId(context)}`; // unique in the ledger
      const applied = await books(ctx).transfer(
        { id, from, to, amount },
        restate.rpc.opts({ idempotencyKey: key(ctx, claude.callId(context)) }),
      );
      return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
    },
  });

const audit = (ctx: restate.SqlObjectContext) =>
  betaZodTool({
    name: 'audit',
    description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
    inputSchema: z.object({}),
    run: async (_args, context) =>
      JSON.stringify(await books(ctx).audit(restate.rpc.opts({ idempotencyKey: key(ctx, claude.callId(context)) }))),
  });

/** Answer a message, once per invocation. */
export const prompt = claude.chat({
  system: 'You keep the books of a bank, with your tools. Answer briefly.',
  tools: [transfer, audit],
});

export const agent = restate.object({
  name: 'ClaudeAgent',
  handlers: { chat: prompt },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

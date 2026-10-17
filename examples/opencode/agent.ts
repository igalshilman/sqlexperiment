// An OpenCode 2.0 agent in a Restate virtual object: an OpenCodeAgent object
// per session, whose OpenCode database (sessions, messages, durable events) is
// the object's database (see with-opencode.ts). OpenCode runs embedded, in
// its Cloudflare Durable Object profile, and its tools move money in the
// ledger (../node/ledger.ts) instead of touching files.
//
// The handler is not replayed: a retried attempt (a crash, a lost connection)
// boots OpenCode again, which resumes the turn it finds unfinished.
// - OpenCode stores each model reply before its tools run. Resuming gives a
//   tool call that was running an "interrupted" result, and asks the model to
//   continue. The model sees the interruption: the transfer tool takes
//   `retryOf`, the id of the call to retry, and uses it as the transfer id, so
//   a retried transfer moves the money once.
// - A tool's call to the ledger has an idempotency key made from the request
//   id (`ctx.request().id`, the same on every attempt) and the transfer id,
//   so a retried attempt reaches the call the first one made.
// - A tool's error fails the attempt, which Restate retries (OpenCode would
//   hand it to the model as the tool's result).
// - A prompt is answered once per prompt id, its OpenCode message id.
//
// Environment: PORT (default 9080), OPENCODE_MODEL (see with-opencode.ts)
// with its provider's API key, and ABORT_PER_MILLE (see ../faults.ts).
// OpenCode needs Node 24.
import * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../node/ledger.ts';
import { tool, withOpenCode } from './with-opencode.ts';

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx: restate.SqlObjectContext) => {
  const books = ctx.objectClient(ledger, 'bank');
  // One idempotency key per tool call: a prompt makes several.
  const key = (callId: string) => `${ctx.request().id}:${callId}`;
  return [
    tool({
      name: 'transfer',
      description:
        'Move an amount of money from one account to another. If an earlier transfer was interrupted, ' +
        'retry it with `retryOf` set to its tool call id: the money then moves once.',
      input: z.object({
        from: z.number().int().describe('Account to take the money from'),
        to: z.number().int().describe('Account to put the money in'),
        amount: z.number().int().min(1),
        retryOf: z.string().optional().describe('The tool call id of the transfer this one retries'),
      }),
      execute: async ({ from, to, amount, retryOf }, toolCallId) => {
        const callId = retryOf ?? toolCallId;
        const id = `OpenCodeAgent/${ctx.key}/${callId}`; // unique in the ledger
        // What moved: a retry of an earlier call gets that call's transfer.
        const applied = await books.transfer(
          { id, from, to, amount },
          restate.rpc.opts({ idempotencyKey: key(callId) }),
        );
        return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
      },
    }),
    tool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      input: z.object({}),
      execute: async (_input, callId) =>
        JSON.stringify(await books.audit(restate.rpc.opts({ idempotencyKey: key(callId) }))),
    }),
  ];
};

/** Answer prompt `text`, once per prompt `id`. */
export const prompt = (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) =>
  withOpenCode(ctx, { tools: bank(ctx) }, async (opencode) => ({
    answer: await opencode.prompt(id, text),
  }));

export const agent = restate.object({
  name: 'OpenCodeAgent',
  handlers: { prompt },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

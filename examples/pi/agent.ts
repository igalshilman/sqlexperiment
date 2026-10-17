// A durable pi agent in a Restate virtual object: an Agent object per session,
// whose pi-durable storage is the object's database (see with-pi.ts).
//
// Its tools move money in the ledger (../node/ledger.ts), with the tool call
// id as the transfer id (prefixed with the agent's key: the ledger is shared,
// and a model's call ids need not be unique beyond its session), so a tool
// call run again after a crash moves the money once. Each call to the ledger has an idempotency key made from the
// request id (`ctx.request().id`, the same on every attempt) and the tool
// call id, so a retried attempt reaches the call the first one made.
//
// Environment: PORT (default 9080), PI_MODEL (see with-pi.ts) with its
// provider's API key, and ABORT_PER_MILLE (see ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { Type } from '@earendil-works/pi-ai';
import { defineTool } from '@earendil-works/pi-durable';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../node/ledger.ts';
import { withPi } from './with-pi.ts';

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx: restate.SqlObjectContext) => {
  const books = ctx.objectClient(ledger, 'bank');
  return [
    defineTool({
      name: 'transfer',
      description: 'Move an amount of money from one account to another.',
      parameters: Type.Object({
        from: Type.Integer({ description: 'Account to take the money from' }),
        to: Type.Integer({ description: 'Account to put the money in' }),
        amount: Type.Integer({ minimum: 1 }),
      }),
      replay: 'safe', // runs again after a crash, with the same call id
      execute: async ({ from, to, amount }, api) => {
        // One key per tool call: an invocation makes many.
        const idempotencyKey = `${ctx.request().id}:${api.callId}`;
        const applied = await books.transfer(
          { id: `Agent/${ctx.key}/${api.callId}`, from, to, amount }, // unique in the ledger
          restate.rpc.opts({ idempotencyKey }),
        );
        return {
          content: [
            { type: 'text', text: `moved ${applied.amount} from account ${applied.from} to account ${applied.to}` },
          ],
        };
      },
    }),
    defineTool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      parameters: Type.Object({}),
      replay: 'safe',
      execute: async (_params, api) => {
        const idempotencyKey = `${ctx.request().id}:${api.callId}`;
        const audit = await books.audit(restate.rpc.opts({ idempotencyKey }));
        return { content: [{ type: 'text', text: JSON.stringify(audit) }] };
      },
    }),
  ];
};

export const agent = restate.object({
  name: 'Agent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: (ctx: restate.SqlObjectContext, { id, text }: { id: string; text: string }) =>
      withPi(ctx, { tools: bank(ctx) }, async (pi) => ({ answer: await pi.prompt(id, text) })),
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

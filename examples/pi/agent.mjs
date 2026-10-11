// A durable pi agent in a Restate virtual object: an Agent object per session,
// whose pi-durable storage is the object's database (see with-pi.mjs).
//
// Its tools move money in the ledger (../node/ledger.mjs), with the tool call
// id as the transfer id, so a tool call run again after a crash moves the
// money once. Each call to the ledger has an idempotency key made from the
// request id (`ctx.request().id`, the same on every attempt) and the tool
// call id, so a retried attempt reaches the call the first one made.
//
// Environment: PORT (default 9080), PI_MODEL (see utils.mjs) and
// ABORT_PER_MILLE (see ../faults.mjs).
import * as restate from '@restatedev/restate-sdk';
import { Type } from '@earendil-works/pi-ai';
import { defineTool } from '@earendil-works/pi-durable';
import { serveIfMain } from '../faults.mjs';
import { ledger } from '../node/ledger.mjs';
import { modelFromEnv } from './utils.mjs';
import { withPi } from './with-pi.mjs';

const { models, model } = await modelFromEnv();

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx) => {
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
        const opts = restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${api.callId}` });
        await books.transfer({ id: api.callId, from, to, amount }, opts);
        return { content: [{ type: 'text', text: `moved ${amount} from account ${from} to account ${to}` }] };
      },
    }),
    defineTool({
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      parameters: Type.Object({}),
      replay: 'safe',
      execute: async (_params, api) => {
        const opts = restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${api.callId}` });
        return { content: [{ type: 'text', text: JSON.stringify(await books.audit(opts)) }] };
      },
    }),
  ];
};

export const agent = restate.object({
  name: 'Agent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: (ctx, { id, text }) =>
      withPi(ctx, { models, model, tools: bank(ctx) }, async (pi) => ({ answer: await pi.prompt(id, text) })),
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

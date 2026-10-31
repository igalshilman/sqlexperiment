// The bank agent on pi (restate-agents/pi): an Agent object per session.
//
// Its tools move money in the ledger (../sql/ledger.ts), with the tool call
// id as the transfer id (prefixed with the agent's key: the ledger is shared,
// and a model's call ids need not be unique beyond its session), so a tool
// call run again after a crash moves the money once. Each call to the ledger has an idempotency key made from the
// request id (`ctx.request().id`, the same on every attempt) and the tool
// call id, so a retried attempt reaches the call the first one made.
//
// Environment: PORT (default 9080), MODEL with its provider's API key,
// and ABORT_PER_MILLE (see ../faults.ts).
import type * as restate from 'restate-sessions';
import { object, signals } from 'restate-agents';
import { Type } from '@earendil-works/pi-ai';
import { defineTool } from '@earendil-works/pi-durable';
import { serveIfMain } from 'restate-sessions/test/faults';
import { ledger } from 'restate-sessions/test/ledger';
import * as pi from 'restate-agents/pi';

/** The bank tools of an invocation: they reach the ledger through `ctx`. */
const transfer = (ctx: restate.SessionContext) =>
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
      const idempotencyKey = `${ctx.request.id}:${api.callId}`;
      const applied = await ctx
        .rpc(ledger, 'bank', { idempotencyKey })
        .transfer({ id: `Agent/${ctx.key}/${api.callId}`, from, to, amount }); // unique in the ledger
      return {
        output: [
          { type: 'text', text: `moved ${applied.amount} from account ${applied.from} to account ${applied.to}` },
        ],
      };
    },
  });

const audit = (ctx: restate.SessionContext) =>
  defineTool({
    name: 'audit',
    description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
    parameters: Type.Object({}),
    replay: 'safe',
    execute: async (_params, api) => {
      const idempotencyKey = `${ctx.request.id}:${api.callId}`;
      const books = await ctx.rpc(ledger, 'bank', { idempotencyKey }).audit();
      return { output: [{ type: 'text', text: JSON.stringify(books) }] };
    },
  });

/** Answer a message, once per invocation. */
export const prompt = pi.chat((ctx) => ({ tools: [transfer(ctx), audit(ctx)] }));

export const agent = object({ name: 'Agent', handlers: { chat: prompt }, views: { messages: pi.messages() } });

serveIfMain(import.meta, [agent, ledger, signals]);

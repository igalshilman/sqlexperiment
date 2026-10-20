// The bank tools, once for every framework (restate-agents' `tool()`): they
// move money in the ledger (../sql/ledger.ts), with the tool call id as the
// transfer id (prefixed with the agent's name and key: the ledger is shared),
// so a tool call run again after a crash moves the money once; and each call
// to the ledger has an idempotency key made from the request id and the tool
// call id, so a retried attempt reaches the call the first one made.
//
// A framework that gives a tool call cut off by a crash a new id (OpenCode,
// the DeepSeek Harness) has the model retry it with `retryOf`, the id of the
// call it retries.
import * as restate from '@restatedev/restate-sdk';
import type * as sql from 'restate-sql';
import { tool } from 'restate-agents';
import { z } from 'zod';
import { ledger } from '../sql/ledger.ts';

/** The ledger. */
const books = (ctx: sql.ObjectContext) => ctx.objectClient(ledger, 'bank');
/** One idempotency key per tool call: a prompt makes several. */
const key = (ctx: sql.ObjectContext, id: string) => `${ctx.request().id}:${id}`;

/** The bank tools of the agent `agent`. */
export const bank = (agent: string) => [
  tool({
    name: 'transfer',
    description:
      'Move an amount of money from one account to another. If an earlier transfer was interrupted, or its ' +
      'outcome is unknown, retry it with `retryOf` set to its tool call id: the money then moves once.',
    input: z.object({
      from: z.number().int().describe('Account to take the money from'),
      to: z.number().int().describe('Account to put the money in'),
      amount: z.number().int().min(1),
      retryOf: z.string().optional().describe('The tool call id of the transfer this one retries'),
    }),
    run: async ({ from, to, amount, retryOf }, { ctx, callId }) => {
      const id = retryOf ?? callId;
      // What moved: a retry of an earlier call gets that call's transfer.
      const applied = await books(ctx).transfer(
        { id: `${agent}/${ctx.key}/${id}`, from, to, amount },
        restate.rpc.opts({ idempotencyKey: key(ctx, id) }),
      );
      return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
    },
  }),
  tool({
    name: 'audit',
    description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
    input: z.object({}),
    run: async (_input, { ctx, callId }) => books(ctx).audit(restate.rpc.opts({ idempotencyKey: key(ctx, callId) })),
  }),
];

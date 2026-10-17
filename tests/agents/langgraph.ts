// The bank agent on LangGraph (restate-agents/langgraph): a GraphAgent object
// per conversation. Its tools move money in the ledger (../sql/ledger.ts),
// with the tool call id as the transfer id, so a tool run again after a crash
// moves the money once; and its call to the ledger has an idempotency key made
// from the request id and the tool call id, so a retried attempt reaches the
// call the first one made.
//
// Environment: PORT (default 9080), ANTHROPIC_API_KEY and ABORT_PER_MILLE (see
// ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { object } from 'restate-agents';
import type { ToolCall } from '@langchain/core/messages/tool';
import { tool } from '@langchain/core/tools';
import * as langgraph from 'restate-agents/langgraph';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../sql/ledger.ts';

/** The id of the tool call a tool runs for: the ToolNode always passes it. */
const callId = (toolCall?: ToolCall) => {
  if (!toolCall?.id) throw new Error('a tool call without an id');
  return toolCall.id;
};

/** The ledger. */
const books = (ctx: restate.SqlObjectContext) => ctx.objectClient(ledger, 'bank');
/** One idempotency key per tool call: a prompt makes several. */
const key = (ctx: restate.SqlObjectContext, id: string) => `${ctx.request().id}:${id}`;

const transfer = (ctx: restate.SqlObjectContext) =>
  tool(
    async ({ from, to, amount }, { toolCall }) => {
      const id = `GraphAgent/${ctx.key}/${callId(toolCall)}`; // unique in the ledger
      const applied = await books(ctx).transfer(
        { id, from, to, amount },
        restate.rpc.opts({ idempotencyKey: key(ctx, callId(toolCall)) }),
      );
      return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
    },
    {
      name: 'transfer',
      description: 'Move an amount of money from one account to another.',
      schema: z.object({
        from: z.number().int().describe('Account to take the money from'),
        to: z.number().int().describe('Account to put the money in'),
        amount: z.number().int().min(1),
      }),
    },
  );

const audit = (ctx: restate.SqlObjectContext) =>
  tool(
    async (_args, { toolCall }) =>
      JSON.stringify(await books(ctx).audit(restate.rpc.opts({ idempotencyKey: key(ctx, callId(toolCall)) }))),
    {
      name: 'audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      schema: z.object({}),
    },
  );

/** Answer a message, once per invocation. */
export const prompt = langgraph.chat({ tools: [transfer, audit] });

export const agent = object({ name: 'GraphAgent', handlers: { chat: prompt } });

serveIfMain(import.meta, [agent, ledger]);

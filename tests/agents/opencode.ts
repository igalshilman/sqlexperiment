// The bank agent on OpenCode 2.0 (restate-agents/opencode): an OpenCodeAgent
// object per session. Its tools move money in the ledger (../sql/ledger.ts)
// instead of touching files.
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
// - A message is answered once per invocation.
//
// Environment: PORT (default 9080), OPENCODE_MODEL with its provider's
// API key, and ABORT_PER_MILLE (see ../faults.ts).
// OpenCode needs Node 24.
import * as restate from '@restatedev/restate-sdk';
import { object } from 'restate-agents';
import { z } from 'zod';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../sql/ledger.ts';
import * as opencode from 'restate-agents/opencode';

/** The ledger, and an idempotency key per tool call: a prompt makes several. */
const books = (ctx: restate.SqlObjectContext) => ctx.objectClient(ledger, 'bank');
const key = (ctx: restate.SqlObjectContext, callId: string) => `${ctx.request().id}:${callId}`;

const transfer = (ctx: restate.SqlObjectContext) =>
  opencode.tool({
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
      const applied = await books(ctx).transfer(
        { id, from, to, amount },
        restate.rpc.opts({ idempotencyKey: key(ctx, callId) }),
      );
      return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
    },
  });

const audit = (ctx: restate.SqlObjectContext) =>
  opencode.tool({
    name: 'audit',
    description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
    input: z.object({}),
    execute: async (_input, callId) =>
      JSON.stringify(await books(ctx).audit(restate.rpc.opts({ idempotencyKey: key(ctx, callId) }))),
  });

/** Answer a message, once per invocation. */
export const prompt = opencode.chat({ tools: [transfer, audit] });

export const agent = object({ name: 'OpenCodeAgent', handlers: { chat: prompt } });

serveIfMain(import.meta, [agent, ledger]);

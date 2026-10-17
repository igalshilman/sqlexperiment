// The bank agent on the DeepSeek Harness (restate-agents/deepseek): a
// DeepSeekAgent object per conversation. Its tools move money in the ledger
// (../sql/ledger.ts). A crash leaves a tool call's outcome unknown, and the
// model sees it: the transfer tool takes `retryOf`, the id of the call to
// retry, and uses it as the transfer id, so a retried transfer moves the money
// once. Its call to the ledger has an idempotency key made from the request id
// and the transfer id, so a retried attempt reaches the call the first one
// made.
//
// Environment: PORT (default 9080), DEEPSEEK_API_KEY and ABORT_PER_MILLE (see
// ../faults.ts).
import * as restate from '@restatedev/restate-sdk';
import { defineTool } from '@deepseek-ai/dsh-tools';
import * as deepseek from 'restate-agents/deepseek';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../sql/ledger.ts';

/** The ledger. */
const books = (ctx: restate.SqlObjectContext) => ctx.objectClient(ledger, 'bank');
/** One idempotency key per tool call: a prompt makes several. */
const key = (ctx: restate.SqlObjectContext, id: string) => `${ctx.request().id}:${id}`;

const transfer = (ctx: restate.SqlObjectContext) =>
  defineTool({
    name: 'transfer',
    description:
      "Move an amount of money from one account to another. If an earlier transfer's outcome is unknown, " +
      'retry it with `retryOf` set to its tool call id: the money then moves once.',
    parameters: {
      from: { type: 'integer', required: true, description: 'Account to take the money from' },
      to: { type: 'integer', required: true, description: 'Account to put the money in' },
      amount: { type: 'integer', required: true },
      retryOf: { type: 'string', description: 'The tool call id of the transfer this one retries' },
    },
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    execute: async ({ from, to, amount, retryOf }, exec) => {
      const callId = retryOf ?? exec.callId;
      const id = `DeepSeekAgent/${ctx.key}/${callId}`; // unique in the ledger
      // What moved: a retry of an earlier call gets that call's transfer.
      const applied = await books(ctx).transfer(
        { id, from, to, amount },
        restate.rpc.opts({ idempotencyKey: key(ctx, callId) }),
      );
      return `moved ${applied.amount} from account ${applied.from} to account ${applied.to}`;
    },
  });

const audit = (ctx: restate.SqlObjectContext) =>
  defineTool({
    name: 'audit',
    description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    execute: async (_args, exec) =>
      JSON.stringify(await books(ctx).audit(restate.rpc.opts({ idempotencyKey: key(ctx, exec.callId) }))),
  });

/** Answer a message, once per invocation. */
export const prompt = deepseek.chat({
  system: 'You keep the books of a bank, with your tools. Answer briefly.',
  tools: [transfer, audit],
});

export const agent = restate.object({
  name: 'DeepSeekAgent',
  handlers: { chat: prompt },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

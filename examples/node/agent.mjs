// A durable pi agent: @earendil-works/pi-durable running in a Restate virtual
// object, with its storage in the object's SQL database.
//
// pi-durable is a durable agent harness: conversations, model turns, tool
// calls and their state are committed to storage before anything is shown,
// and reopening the storage picks unfinished work up where it stopped. Here
// its storage is the object's database: the handler runs in one
// `ctx.sqlSession`, and each pi-durable commit is an SQL transaction in it
// (pi-storage.mjs), stored in order with everything else the handler does. So an Agent object per session keeps the
// whole harness state, and a retried attempt (a crash, a lost connection)
// reopens it and carries on:
// - a prompt is submitted once, by its id (pi-durable's requestId);
// - an answered prompt returns its answer;
// - unfinished work resumes: a model call is made again, and a tool call
//   declared replay-safe runs again with the same call id.
//
// The tools move money in the ledger, with the call id as the transfer id, and
// the call to the ledger gets an idempotency key derived by the SDK.
//
// The model: PI_MODEL=provider/model-id (such as anthropic/claude-sonnet-4-5,
// with the provider's API key), or by default a scripted teller that needs no
// key: "move 5 from 1 to 2" makes a transfer, "audit" audits the ledger.
//
// Environment: PORT (default 9080), PI_MODEL and ABORT_PER_MILLE (see faults.mjs).
import * as restate from '@restatedev/restate-sdk';
import { BACKGROUND_CONTEXT } from '@earendil-works/chord/context';
import { Type } from '@earendil-works/pi-ai';
import { createModels } from '@earendil-works/pi-ai/models';
import { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall } from '@earendil-works/pi-ai/providers/faux';
import { AssistantEntry, createRegistry, defineExtension, defineTool, Harness } from '@earendil-works/pi-durable';
import { serveIfMain } from './faults.mjs';
import { ledger } from './ledger.mjs';
import { openSqlKvStorage } from './pi-storage.mjs';

const text = (content) =>
  typeof content === 'string'
    ? content
    : content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

/** The scripted teller, as a pi-ai provider: one reply per model call. */
function teller() {
  const faux = fauxProvider({ provider: 'teller', models: [{ id: 'teller' }] });
  const reply = (context) => {
    faux.appendResponses([reply]); // and one for the next call
    const last = context.messages.at(-1);
    if (last.role === 'toolResult') return fauxAssistantMessage(fauxText(`Done: ${text(last.content)}`));
    const move = /move (\d+) from (\d+) to (\d+)/i.exec(text(last.content));
    if (move) {
      const [amount, from, to] = move.slice(1).map(Number);
      return fauxAssistantMessage([fauxText('On it.'), fauxToolCall('transfer', { from, to, amount })], {
        stopReason: 'toolUse',
      });
    }
    if (/audit/i.test(text(last.content))) {
      return fauxAssistantMessage(fauxToolCall('audit', {}), { stopReason: 'toolUse' });
    }
    return fauxAssistantMessage(fauxText('Ask me to "move 5 from 1 to 2", or to "audit" the books.'));
  };
  faux.setResponses([reply]);
  return faux.provider;
}

const models = createModels();
const [provider, modelId] = process.env.PI_MODEL ? process.env.PI_MODEL.split('/') : ['teller', 'teller'];
if (provider === 'teller') {
  models.setProvider(teller());
} else {
  // Such as anthropicProvider() from @earendil-works/pi-ai/providers/anthropic.
  const module = await import(`@earendil-works/pi-ai/providers/${provider}`);
  models.setProvider(module[`${provider}Provider`]());
}

/** The bank tools of one invocation: they reach the ledger through `ctx`. */
const bank = (ctx) => {
  const books = ctx.objectClient(ledger, 'bank');
  return defineExtension({
    name: 'bank',
    tools: [
      defineTool({
        name: 'transfer',
        description: 'Move an amount of money from one account to another.',
        parameters: Type.Object({
          from: Type.Integer({ description: 'Account to take the money from' }),
          to: Type.Integer({ description: 'Account to put the money in' }),
          amount: Type.Integer({ minimum: 1 }),
        }),
        // Runs again after a crash: the call id is the transfer id, so the
        // ledger applies it once.
        replay: 'safe',
        execute: async ({ from, to, amount }, api) => {
          await books.transfer({ id: api.callId, from, to, amount });
          return { content: [{ type: 'text', text: `moved ${amount} from account ${from} to account ${to}` }] };
        },
      }),
      defineTool({
        name: 'audit',
        description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
        parameters: Type.Object({}),
        replay: 'safe',
        execute: async () => ({ content: [{ type: 'text', text: JSON.stringify(await books.audit()) }] }),
      }),
    ],
  });
};

/** Run `work` with a Harness over this object's database, for this invocation. */
const withHarness = (ctx, work) =>
  ctx.sqlSession(async (db) => {
    const context = BACKGROUND_CONTEXT;
    const registry = createRegistry();
    registry.install(bank(ctx));
    const harness = await Harness.open(await openSqlKvStorage(db), { models, registry }, context);
    try {
      return await work(harness, context);
    } finally {
      await harness.close(context);
    }
  });

export const agent = restate.object({
  name: 'Agent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: (ctx, { id, text: prompt }) =>
      withHarness(ctx, async (harness, context) => {
        const root = await harness.root(context, { agent: { model: { provider, modelId } } });
        const submission = await root.submit({ type: 'input', content: prompt, requestId: id }, context);
        const settled = await submission.wait(context);
        if (settled.status !== 'done') {
          throw new restate.TerminalError(`prompt ${id} unanswered: ${settled.reason ?? 'unknown'}`);
        }
        const answer = await root.commit((tx) => tx.entry(AssistantEntry, settled.answer), context);
        return { answer: text(answer.model.at(-1).content) };
      }),
  },
  options: {
    ...restate.sqlOptions,
    // A prompt is long work, and each attempt resumes from storage: keep
    // retrying rather than pause after Restate's default number of attempts.
    retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

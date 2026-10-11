// A durable pi agent (@mariozechner/pi-agent-core): an Agent object per
// session, whose transcript lives in the object's SQL database, with tools
// that move money in the ledger.
//
// Every message is stored as the loop produces it, and before any tool runs.
// A retried attempt (a crash, a lost connection) loads the transcript and
// carries on from where it stopped:
// - a prompt is added once, by its id (part of the request);
// - a prompt already answered returns its answer, without the model;
// - tool calls of the last assistant message that have no results yet run
//   again with the same tool call id, which the ledger dedupes on, and the
//   call to the ledger, with an idempotency key the SDK derives, reaches the
//   same invocation;
// - otherwise the loop continues from the transcript.
// A model call cut short is simply made again.
//
// The model: PI_MODEL=provider/model-id (with that provider's API key, such
// as ANTHROPIC_API_KEY), or by default a scripted teller that needs no key:
// "move 5 from 1 to 2" makes a transfer, "audit" audits the ledger.
//
// Environment: PORT (default 9080), PI_MODEL and ABORT_PER_MILLE (see faults.mjs).
import * as restate from '@restatedev/restate-sdk';
import { runAgentLoopContinue } from '@mariozechner/pi-agent-core';
import {
  fauxAssistantMessage,
  fauxText,
  fauxToolCall,
  getModel,
  registerFauxProvider,
  streamSimple,
} from '@mariozechner/pi-ai';
import { Type } from 'typebox';
import { serveIfMain } from './faults.mjs';
import { ledger } from './ledger.mjs';

const SYSTEM_PROMPT =
  'You are a bank teller. Move money between accounts with the transfer tool, and check the books with the audit tool.';

const text = (message) =>
  typeof message.content === 'string'
    ? message.content
    : message.content
        .filter((c) => c.type === 'text')
        .map((c) => c.text)
        .join('');

/** The scripted teller: the model when PI_MODEL is not set. */
function teller() {
  const faux = registerFauxProvider({ provider: 'teller', models: [{ id: 'teller' }] });
  const reply = (context) => {
    const last = context.messages.at(-1);
    if (last.role === 'toolResult') return fauxAssistantMessage(fauxText(`Done: ${text(last)}`));
    const move = /move (\d+) from (\d+) to (\d+)/i.exec(text(last));
    if (move) {
      const [amount, from, to] = move.slice(1).map(Number);
      return fauxAssistantMessage([fauxText('On it.'), fauxToolCall('transfer', { from, to, amount })], {
        stopReason: 'toolUse',
      });
    }
    if (/audit/i.test(text(last))) {
      return fauxAssistantMessage(fauxToolCall('audit', {}), { stopReason: 'toolUse' });
    }
    return fauxAssistantMessage(fauxText('Ask me to "move 5 from 1 to 2", or to "audit" the books.'));
  };
  return {
    model: faux.getModel(),
    // One scripted reply per model call.
    streamFn: (...args) => {
      faux.setResponses([reply]);
      return streamSimple(...args);
    },
  };
}

const [provider, modelId] = (process.env.PI_MODEL ?? '').split('/');
const llm = provider && modelId ? { model: getModel(provider, modelId) } : teller();

/** The tools of one invocation: they reach the ledger through `ctx`. */
function tools(ctx) {
  const bank = ctx.objectClient(ledger, 'bank');
  const result = (text, details) => ({ content: [{ type: 'text', text }], details });
  return [
    {
      name: 'transfer',
      label: 'Transfer',
      description: 'Move an amount of money from one account to another.',
      parameters: Type.Object({
        from: Type.Integer({ description: 'Account to take the money from' }),
        to: Type.Integer({ description: 'Account to put the money in' }),
        amount: Type.Integer({ minimum: 1 }),
      }),
      // The tool call id is the transfer id: stable once the assistant message
      // is stored, so a transfer that runs again is applied once.
      execute: async (toolCallId, { from, to, amount }) => {
        await bank.transfer({ id: toolCallId, from, to, amount });
        return result(`moved ${amount} from account ${from} to account ${to}`, { from, to, amount });
      },
    },
    {
      name: 'audit',
      label: 'Audit',
      description: 'Check the books: accounts, total money, transfers, and inconsistent accounts.',
      parameters: Type.Object({}),
      execute: async () => {
        const audit = await bank.audit();
        return result(JSON.stringify(audit), audit);
      },
    },
  ];
}

const TRANSCRIPT = 'CREATE TABLE IF NOT EXISTS transcript(seq INTEGER PRIMARY KEY, prompt TEXT NOT NULL, message TEXT NOT NULL)';

/** The stored transcript: `{ prompt, message }` in order. */
const load = (ctx) =>
  ctx.sql(async (db) => {
    await db.execute(TRANSCRIPT);
    const rows = await db.query('SELECT prompt, message FROM transcript ORDER BY seq');
    return rows.map(([prompt, message]) => ({ prompt, message: JSON.parse(message) }));
  });

/** Store `message`, of prompt `id`: durable once this resolves. */
const store = (ctx, id, message) =>
  ctx.sql((db) => db.execute('INSERT INTO transcript(prompt, message) VALUES(?, ?)', [id, JSON.stringify(message)]));

const toolCalls = (message) =>
  message.role === 'assistant' ? message.content.filter((c) => c.type === 'toolCall') : [];

/** Run the tool calls of `assistant` that the transcript has no results for. */
async function runPendingTools(ctx, id, transcript, assistant, toolset) {
  const done = new Set(transcript.filter((e) => e.message.role === 'toolResult').map((e) => e.message.toolCallId));
  for (const call of toolCalls(assistant)) {
    if (done.has(call.id)) continue;
    const tool = toolset.find((t) => t.name === call.name);
    let outcome;
    try {
      if (!tool) throw new Error(`no tool ${call.name}`);
      outcome = { ...(await tool.execute(call.id, call.arguments)), isError: false };
    } catch (error) {
      outcome = { content: [{ type: 'text', text: String(error?.message ?? error) }], details: {}, isError: true };
    }
    const message = { role: 'toolResult', toolCallId: call.id, toolName: call.name, ...outcome, timestamp: Date.now() };
    await store(ctx, id, message);
    transcript.push({ prompt: id, message });
  }
}

export const agent = restate.object({
  name: 'Agent',
  handlers: {
    /** Answer prompt `text`, once per prompt `id`. */
    prompt: async (ctx, { id, text: prompt }) => {
      const transcript = await load(ctx);
      const mine = transcript.filter((e) => e.prompt === id).map((e) => e.message);
      if (mine.length === 0) {
        const message = { role: 'user', content: prompt, timestamp: Date.now() };
        await store(ctx, id, message);
        transcript.push({ prompt: id, message });
      } else {
        const last = mine.at(-1);
        if (last.role === 'assistant' && toolCalls(last).length === 0) return { answer: text(last) };
      }

      // A crash after the model asked for tools, but before their results.
      const toolset = tools(ctx);
      const last = transcript.at(-1).message;
      if (toolCalls(last).length > 0) await runPendingTools(ctx, id, transcript, last, toolset);

      let answer = '';
      await runAgentLoopContinue(
        { systemPrompt: SYSTEM_PROMPT, messages: transcript.map((e) => e.message), tools: toolset },
        { model: llm.model, convertToLlm: (messages) => messages, toolExecution: 'sequential' },
        async (event) => {
          if (event.type !== 'message_end') return;
          const message = event.message;
          if (message.role === 'assistant' && (message.stopReason === 'error' || message.stopReason === 'aborted')) {
            // Not stored: the attempt is retried, and asks the model again.
            throw new Error(`model error: ${message.errorMessage ?? message.stopReason}`);
          }
          // Stored before any tool of it runs: the loop awaits this.
          await store(ctx, id, message);
          if (message.role === 'assistant') answer = text(message);
        },
        undefined,
        llm.streamFn,
      );
      return { answer };
    },

    /** The transcript, for display. */
    transcript: async (ctx) => (await load(ctx)).map(({ prompt, message }) => ({ prompt, role: message.role, text: text(message) })),
  },
  options: {
    ...restate.sqlOptions,
    retryPolicy: { initialInterval: 50, maxInterval: 500 },
  },
});

serveIfMain(import.meta, [agent, ledger]);

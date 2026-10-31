// The bank agent on the DeepSeek Harness (restate-agents/deepseek): a DeepSeekAgent object per conversation, whose tools
// (./bank.ts) move money in the ledger.
//
// Environment: PORT (default 9080), DEEPSEEK_API_KEY, MODEL, and ABORT_PER_MILLE (see
// ../faults.ts).
import { object, signals } from 'restate-agents';
import * as deepseek from 'restate-agents/deepseek';
import { serveIfMain } from 'restate-objects/test/faults';
import { ledger } from 'restate-objects/test/ledger';
import { bank } from './bank.ts';

/** Answer a message, once per invocation. */
export const prompt = deepseek.chat({
  system: 'You keep the books of a bank, with your tools. Answer briefly.',
  tools: bank('DeepSeekAgent'),
});

export const agent = object({
  name: 'DeepSeekAgent',
  handlers: { chat: prompt },
  views: { messages: deepseek.messages() },
});

serveIfMain(import.meta, [agent, ledger, signals]);

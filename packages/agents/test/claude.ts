// The bank agent on Claude (restate-agents/claude): a ClaudeAgent object per conversation, whose tools
// (./bank.ts) move money in the ledger.
//
// Environment: PORT (default 9080), ANTHROPIC_API_KEY, MODEL, and ABORT_PER_MILLE (see
// ../faults.ts).
import { object, signals } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import { serveIfMain } from 'restate-objects/test/faults';
import { ledger } from 'restate-objects/test/ledger';
import { bank } from './bank.ts';

/** Answer a message, once per invocation. */
export const prompt = claude.chat({
  system: 'You keep the books of a bank, with your tools. Answer briefly.',
  tools: bank('ClaudeAgent'),
});

export const agent = object({ name: 'ClaudeAgent', handlers: { chat: prompt, messages: claude.messages() } });

serveIfMain(import.meta, [agent, ledger, signals]);

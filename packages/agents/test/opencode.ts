// The bank agent on OpenCode 2.0 (restate-agents/opencode): an OpenCodeAgent object per session, whose tools
// (./bank.ts) move money in the ledger.
//
// Environment: PORT (default 9080), OPENAI_API_KEY, MODEL, and ABORT_PER_MILLE (see
// ../faults.ts).
import { object, signals } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';
import { serveIfMain } from 'restate-fns/test/faults';
import { ledger } from 'restate-fns/test/ledger';
import { bank } from './bank.ts';

/** Answer a message, once per invocation. */
export const prompt = opencode.chat({ tools: bank('OpenCodeAgent') });

export const agent = object({ name: 'OpenCodeAgent', handlers: { chat: prompt, messages: opencode.messages() } });

serveIfMain(import.meta, [agent, ledger, signals]);

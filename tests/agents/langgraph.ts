// The bank agent on LangGraph (restate-agents/langgraph): a GraphAgent object per conversation, whose tools
// (./bank.ts) move money in the ledger.
//
// Environment: PORT (default 9080), ANTHROPIC_API_KEY, MODEL, and ABORT_PER_MILLE (see
// ../faults.ts).
import { object, signals } from 'restate-agents';
import * as langgraph from 'restate-agents/langgraph';
import { serveIfMain } from '../faults.ts';
import { ledger } from '../sql/ledger.ts';
import { bank } from './bank.ts';

/** Answer a message, once per invocation. */
export const prompt = langgraph.chat({ tools: bank('GraphAgent') });

export const agent = object({ name: 'GraphAgent', handlers: { chat: prompt, messages: langgraph.messages() } });

serveIfMain(import.meta, [agent, ledger, signals]);

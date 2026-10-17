/**
 * Agents on models you set up yourself: Claude through an Anthropic client of
 * your own, and LangGraph on a LangChain chat model of your own.
 *
 * - `precise` (Claude): the client takes a gateway's address, if you have one
 *   (ANTHROPIC_BASE_URL), and makes no retries of its own: when the API
 *   fails, the attempt fails, and Restate retries it, resuming the
 *   conversation from the agent's database. Its replies are short
 *   (`maxTokens`).
 * - `creative` (LangGraph): a ChatAnthropic of its own settings, here a high
 *   temperature. Any LangChain chat model that calls tools works the same.
 *
 *   ANTHROPIC_API_KEY=… node models.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/precise/alice/chat -d 'Name a color.'
 *   curl localhost:8080/creative/alice/chat -d 'Name a color.'
 */
import Anthropic from '@anthropic-ai/sdk';
import { ChatAnthropic } from '@langchain/anthropic';
import { serve } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import * as langgraph from 'restate-agents/langgraph';

const precise = claude.agent({
  name: 'precise',
  client: new Anthropic({ baseURL: process.env.ANTHROPIC_BASE_URL, maxRetries: 0 }),
  maxTokens: 512,
  system: 'Answer in as few words as you can.',
});

const creative = langgraph.agent({
  name: 'creative',
  model: new ChatAnthropic({ model: 'claude-sonnet-5-5', temperature: 1 }),
  system: 'Answer with imagination.',
});

await serve(precise, creative);

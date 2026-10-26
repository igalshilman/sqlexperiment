/**
 * Agents on models you set up yourself: LangChain chat models of your own,
 * for LangGraph agents. Any LangChain chat model that calls tools works the
 * same.
 *
 * - `precise`: the client takes a gateway's address, if you have one
 *   (OPENAI_BASE_URL), and makes no retries of its own: when the API fails,
 *   the attempt fails, and Restate retries it, resuming the conversation from
 *   the agent's database. Its replies are short (`maxTokens`).
 * - `creative`: a model of its own settings, here a high temperature.
 *
 *   OPENAI_API_KEY=… node basic/models.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/precise/alice/chat -d 'Name a color.'
 *   curl localhost:8080/creative/alice/chat -d 'Name a color.'
 */
import { ChatOpenAI } from '@langchain/openai';
import { serve } from 'restate-agents';
import * as langgraph from 'restate-agents/langgraph';

const precise = langgraph.agent({
  name: 'precise',
  model: new ChatOpenAI({
    model: 'gpt-6-sol',
    configuration: { baseURL: process.env.OPENAI_BASE_URL },
    maxRetries: 0,
    maxTokens: 512,
  }),
  system: 'Answer in as few words as you can.',
});

const creative = langgraph.agent({
  name: 'creative',
  model: new ChatOpenAI({ model: 'gpt-6-sol', temperature: 1 }),
  system: 'Answer with imagination.',
});

await serve(precise, creative);

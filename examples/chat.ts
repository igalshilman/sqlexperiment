/**
 * A durable chat agent on pi.
 *
 * Each key is a conversation of its own (`assistant/alice`, `assistant/bob`),
 * kept in the object's SQL database. If the process dies mid-reply, Restate
 * retries the call and the agent picks up where it left off.
 *
 *   ANTHROPIC_API_KEY=… node chat.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
 *   curl localhost:8080/assistant/alice/chat -d 'What is my name?'
 */
import { Type } from '@earendil-works/pi-ai';
import { defineTool } from '@earendil-works/pi-durable';
import { serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const weather = defineTool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  parameters: Type.Object({ city: Type.String() }),
  // A stand-in for a real weather API.
  execute: async ({ city }) => ({ content: [{ type: 'text', text: `Sunny and 21°C in ${city}` }] }),
});

const assistant = pi.agent({ name: 'assistant', tools: [weather] });

await serve(assistant);

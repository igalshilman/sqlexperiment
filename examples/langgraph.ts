/**
 * A durable chat agent on LangGraph: a tool-calling graph whose checkpoints
 * live in the object's SQL database.
 *
 * Each key is a thread of its own. If the process dies mid-reply, Restate
 * retries the call and the graph carries on from its last step.
 *
 *   ANTHROPIC_API_KEY=… node langgraph.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat --json '"Hi, I am Alice. Is it warm in Lisbon?"'
 */
import * as restate from '@restatedev/restate-sdk';
import { tool } from '@langchain/core/tools';
import * as langgraph from 'restate-agents/langgraph';
import { z } from 'zod';

const weather = tool(
  // A stand-in for a real weather API.
  async ({ city }) => `Sunny and 21°C in ${city}`,
  {
    name: 'get_weather',
    description: 'The current weather in a city.',
    schema: z.object({ city: z.string() }),
  },
);

const assistant = restate.object({
  name: 'assistant',
  handlers: {
    chat: langgraph.chat({ tools: [weather] }),
  },
  options: restate.sqlOptions,
});

await restate.serve({ services: [assistant] });

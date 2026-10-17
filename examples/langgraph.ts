/**
 * A durable chat agent on LangGraph: a tool-calling graph whose checkpoints
 * live in the object's SQL database.
 *
 * Each key is a thread of its own. If the process dies mid-reply, Restate
 * retries the call and the graph carries on from its last step.
 *
 *   ANTHROPIC_API_KEY=… node langgraph.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
 */
import { serve, tool } from 'restate-agents';
import * as langgraph from 'restate-agents/langgraph';
import { z } from 'zod';

// A tool, for any framework's agent (the framework's own tools work too).
const weather = tool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  input: z.object({ city: z.string() }),
  // A stand-in for a real weather API.
  run: async ({ city }) => `Sunny and 21°C in ${city}`,
});

const assistant = langgraph.agent({ name: 'assistant', tools: [weather] });

await serve(assistant);

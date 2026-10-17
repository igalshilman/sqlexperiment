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
import { serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

// A tool, for any framework's agent (the framework's own tools work too).
const weather = tool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  input: z.object({ city: z.string() }),
  // A stand-in for a real weather API.
  run: async ({ city }) => `Sunny and 21°C in ${city}`,
});

const assistant = pi.agent({ name: 'assistant', tools: [weather] });

await serve(assistant);

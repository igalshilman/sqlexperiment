/**
 * A durable chat agent on OpenCode 2.0, embedded in the handler: its
 * sessions, messages and events live in the object's SQL database.
 *
 * Each key is a conversation of its own. If the process dies mid-reply,
 * Restate retries the call and OpenCode resumes the turn it was in.
 *
 *   ANTHROPIC_API_KEY=… node opencode.ts
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
 */
import { serve, tool } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';
import { z } from 'zod';

// A tool, for any framework's agent (the framework's own tools work too).
const weather = tool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  input: z.object({ city: z.string() }),
  // A stand-in for a real weather API.
  run: async ({ city }) => `Sunny and 21°C in ${city}`,
});

const assistant = opencode.agent({ name: 'assistant', tools: [weather] });

await serve(assistant);

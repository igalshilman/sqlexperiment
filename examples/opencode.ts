/**
 * A durable chat agent on OpenCode 2.0, embedded in the handler: its
 * sessions, messages and events live in the object's SQL database.
 *
 * Each key is a conversation of its own. If the process dies mid-reply,
 * Restate retries the call and OpenCode resumes the turn it was in.
 *
 *   ANTHROPIC_API_KEY=… node opencode.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat --json '"Hi, I am Alice. Is it warm in Lisbon?"'
 */
import * as restate from '@restatedev/restate-sdk';
import * as opencode from 'restate-agents/opencode';
import { z } from 'zod';

const weather = opencode.tool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  input: z.object({ city: z.string() }),
  // A stand-in for a real weather API.
  execute: async ({ city }) => `Sunny and 21°C in ${city}`,
});

const assistant = restate.object({
  name: 'assistant',
  handlers: {
    chat: opencode.chat({ tools: [weather] }),
  },
  options: restate.sqlOptions,
});

await restate.serve({ services: [assistant] });

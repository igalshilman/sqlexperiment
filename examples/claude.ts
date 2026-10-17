/**
 * A durable chat agent on Claude, with the Anthropic SDK's tool runner: the
 * conversation lives in the object's SQL database.
 *
 * Each key is a conversation of its own. If the process dies mid-reply,
 * Restate retries the call and the runner carries on from the stored
 * messages.
 *
 *   ANTHROPIC_API_KEY=… node claude.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat --json '"Hi, I am Alice. Is it warm in Lisbon?"'
 */
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import { serve } from 'restate-agents';
import * as claude from 'restate-agents/claude';
import { z } from 'zod';

const weather = betaZodTool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  inputSchema: z.object({ city: z.string() }),
  // A stand-in for a real weather API.
  run: async ({ city }) => `Sunny and 21°C in ${city}`,
});

const assistant = claude.agent({
  name: 'assistant',
  system: 'You are a friendly assistant. Answer briefly.',
  tools: [weather],
});

await serve(assistant);

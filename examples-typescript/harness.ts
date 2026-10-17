/**
 * An assistant each user sets up as it runs: each user (the object's key)
 * picks its model and persona, kept in the object's database, which `chat`
 * reads for each message (`pi.chat(options)`, with options of the
 * invocation); `compact` has the model summarize the conversation so far, to
 * keep its context short; and `reset` starts the conversation over, with a
 * note to carry on (both on pi's Harness directly, `withPi`).
 *
 *   ANTHROPIC_API_KEY=… node harness.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/configure --json '{"persona": "You answer as a pirate."}'
 *   curl localhost:8080/assistant/alice/chat -d 'What time is it?'
 *   curl localhost:8080/assistant/alice/compact -d 'Keep the times you were asked'
 *   curl localhost:8080/assistant/alice/reset -d 'Alice likes short answers'
 *   curl localhost:8080/assistant/alice/messages
 *
 * A model of another provider takes its API key, such as
 * `{"model": "openai/gpt-6-luna"}` with OPENAI_API_KEY.
 */
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import type * as restate from '@restatedev/restate-sdk';
import { z } from 'zod';

/** What a user sets up: the model, as `provider/model-id`, and a persona, the system prompt. */
interface Settings {
  model?: string;
  persona?: string;
}

const SCHEMA = 'CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 0), json TEXT NOT NULL)';

async function settingsOf(ctx: restate.SqlObjectContext): Promise<Settings> {
  await ctx.db.execute(SCHEMA);
  const [row] = await ctx.db.query('SELECT json FROM settings');
  return row ? (JSON.parse(String(row[0])) as Settings) : {};
}

const clock = tool({
  name: 'clock',
  description: 'The current time, in UTC.',
  input: z.object({}),
  run: async () => new Date().toISOString(),
});

/** pi's options for the user of `ctx`: its model and persona, and the tools. */
const options = async (ctx: restate.SqlObjectContext): Promise<pi.Options> => {
  const { model, persona } = await settingsOf(ctx);
  return { model, system: persona ?? 'You are a helpful assistant.', tools: [clock] };
};

const assistant = object({
  name: 'assistant',
  handlers: {
    chat: pi.chat(options),
    messages: pi.messages(),

    /** Set the model or the persona (or both) for the next messages. */
    configure: async (ctx, settings: Settings) => {
      const next = { ...(await settingsOf(ctx)), ...settings };
      await ctx.db.execute('INSERT OR REPLACE INTO settings (id, json) VALUES (0, ?)', [JSON.stringify(next)]);
      return next;
    },

    /** Summarize the conversation so far, as `instructions` say, in place of it. */
    compact: async (ctx, instructions?: string) =>
      pi.withPi(ctx, await options(ctx), async ({ conversation, context }) => {
        const root = await conversation();
        await root.compact(instructions || undefined, context);
        await root.waitForIdle(context);
      }),

    /** Start the conversation over, with `handoff` as what to carry on with. */
    reset: async (ctx, handoff?: string) =>
      pi.withPi(ctx, await options(ctx), async ({ conversation, context }) =>
        (await conversation()).reset(handoff || undefined, context),
      ),
  },
});

await serve(assistant);

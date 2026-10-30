/**
 * An assistant with a memory: the facts it learns about you go into a table
 * of its own SQL database, next to its conversation. Its tools write and read
 * them through `ctx.db`, and a handler of your own serves them, for a profile
 * page say.
 *
 *   OPENAI_API_KEY=… node basic/memory.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
 *   curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
 *   curl localhost:8080/assistant/alice/facts
 */
import { handlers } from 'restate-objects';
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user, for this conversation and later ones.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute('INSERT OR IGNORE INTO facts (fact) VALUES (?)', [fact]);
    return `Remembered: ${fact}`;
  },
});

const recall = tool({
  name: 'recall',
  description: 'Everything remembered about the user.',
  input: z.object({}),
  run: async (_input, { ctx }) => {
    const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
    return rows.map(([fact]) => fact);
  },
});

// An object of your own: the agent's handlers, and yours beside them.
const assistant = object({
  name: 'assistant',
  // A table of your own, beside the agent's: the facts, in each user's database.
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'],
  handlers: {
    chat: pi.chat({
      system: 'You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.',
      tools: [remember, recall],
    }),
    messages: pi.messages(),

    /** What the assistant knows about the user: shared, so it answers while a message is answered. */
    facts: handlers.object.shared(async (ctx) => {
      const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(([fact]) => fact);
    }),

    /** Forget a fact. */
    forget: async (ctx, fact: string) => {
      await ctx.db.execute('DELETE FROM facts WHERE fact = ?', [fact]);
    },
  },
});

await serve(assistant);

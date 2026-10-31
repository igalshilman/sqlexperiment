/**
 * An assistant with a memory: the facts it learns about you go into a table
 * of its own SQL database, next to its conversation. Its tools write and read
 * them through `ctx.db`, and a view of your own serves them, for a profile
 * page say.
 *
 *   OPENAI_API_KEY=… node basic/memory.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
 *   curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
 *   curl localhost:8080/assistant/alice/facts
 */
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
    const rows = await ctx.db.query<{ fact: string }>('SELECT fact FROM facts ORDER BY rowid');
    return rows.map(({ fact }) => fact);
  },
});

// An object of your own: the agent's handler and view, and yours beside them.
const assistant = object({
  name: 'assistant',
  // Your own table, in each user's database next to the agent's tables.
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'],
  handlers: {
    chat: pi.chat({
      system: 'You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.',
      tools: [remember, recall],
    }),
    /** Forget a fact. */
    forget: async (ctx, fact: string) => {
      await ctx.db.execute('DELETE FROM facts WHERE fact = ?', [fact]);
    },
  },
  views: {
    messages: pi.messages(),

    /** What the assistant knows about the user. A view, so it answers even while the agent is busy. */
    facts: async (ctx) => {
      const rows = await ctx.db.query<{ fact: string }>('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(({ fact }) => fact);
    },
  },
});

await serve(assistant);

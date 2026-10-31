/**
 * 12 · An agent in your own object
 *
 * `object` holds the agent's handler and view (`pi.chat`, `pi.messages`)
 * beside handlers, views and a schema of your own: one database for the
 * conversation and your tables. A tool's `run` gets the handler's `ctx`, so
 * it reaches that database too. Here, the agent remembers facts about the
 * user, and the view `facts` serves them, for a profile page say.
 *
 *   OPENAI_API_KEY=… node src/12_memory.ts      # and register it (see 01)
 *   curl localhost:8080/companion/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
 *   curl localhost:8080/companion/alice/facts   # what it remembered
 */
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute('INSERT OR IGNORE INTO facts (fact) VALUES (?)', [fact]);
    return 'Remembered.';
  },
});

const companion = object({
  name: 'companion',
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'],
  handlers: {
    chat: pi.chat({ system: 'Remember what you learn about the user.', tools: [remember] }),
  },
  views: {
    messages: pi.messages(),

    facts: async (ctx) => {
      const rows = await ctx.db.query<{ fact: string }>('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(({ fact }) => fact);
    },
  },
});

await serve(companion);

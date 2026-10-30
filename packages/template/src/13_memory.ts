/**
 * 13 · An agent in your own object
 *
 * `object` holds the agent's handlers (`pi.chat`, `pi.messages`) beside
 * handlers of your own, and a schema of your own: one database for the
 * conversation and your tables. A tool's `run` gets the handler's `ctx`, so
 * it reaches that database too. Here, the agent remembers facts about the
 * user, and `facts` serves them, for a profile page say.
 *
 *   node src/13_memory.ts                       # and register it (see 01)
 *   curl localhost:8080/companion/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
 *   curl localhost:8080/companion/alice/facts   # what it remembered
 */
import { handlers } from 'restate-objects';
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
    messages: pi.messages(),

    facts: handlers.object.shared(async (ctx) => {
      const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(([fact]) => fact);
    }),
  },
});

await serve(companion);

/**
 * 04 · A schema that grows
 *
 * `schema` is a list of steps, applied in order, once per key. To change the
 * tables, add a step: each key's database runs the steps it lacks the next
 * time it is used. A step that ran never changes.
 *
 *   node src/04_schema.ts                                         # and register it (see 01)
 *   curl localhost:8080/greeter/alice/greet --json '"cheerful"'   # "Hello, alice! You look cheerful."
 *   curl localhost:8080/greeter/alice/history                     # the greetings of 02 and 03 have no mood
 */
import * as restate from 'restate-sessions';

const greeter = restate.sessionObject({
  name: 'greeter',
  schema: [
    'CREATE TABLE greetings (at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)', // 02's
    'ALTER TABLE greetings ADD COLUMN mood TEXT', // new: runs once on each key's database
  ],
  handlers: {
    greet: async (ctx, mood: string) => {
      await ctx.db.execute('INSERT INTO greetings (mood) VALUES (?)', [mood]);
      return `Hello, ${ctx.key}! You look ${mood}.`;
    },
  },
  views: {
    history: async (ctx) => ctx.db.query<{ at: string; mood: string }>('SELECT at, mood FROM greetings ORDER BY rowid'),
  },
});

await restate.serve({ services: [greeter], port: 9080 });

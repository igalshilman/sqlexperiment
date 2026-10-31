/**
 * 02 · A database per key
 *
 * Each key of an object has a SQLite database of its own, `ctx.db`, kept in
 * the object's state in Restate: there is no database server to run.
 * `schema` creates its tables, once per key, when a handler of the key first
 * uses it.
 *
 *   node src/02_database.ts                   # and register it (see 01)
 *   curl localhost:8080/greeter/alice/greet   # "Hello, alice! (greeting 1)"
 *   curl localhost:8080/greeter/alice/greet   # "Hello, alice! (greeting 2)"
 *   curl localhost:8080/greeter/bob/greet     # "Hello, bob! (greeting 1)": another key, another database
 */
import * as restate from 'restate-objects';

const greeter = restate.sessionObject({
  name: 'greeter',
  schema: ['CREATE TABLE greetings (at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)'],
  handlers: {
    greet: async (ctx) => {
      await ctx.db.execute('INSERT INTO greetings DEFAULT VALUES');
      const greetings = await ctx.db.query('SELECT at FROM greetings'); // rows, each an object by column name
      return `Hello, ${ctx.key}! (greeting ${greetings.length})`;
    },
  },
});

await restate.serve({ services: [greeter], port: 9080 });

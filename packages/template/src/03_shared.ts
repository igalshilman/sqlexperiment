/**
 * 03 · Reading beside the writer: shared handlers
 *
 * A key runs one handler at a time, so its writes never race. A shared
 * handler is the exception: it runs whenever it is called, even while
 * another handler of the key runs, and reads the database as stored. It
 * writes nothing.
 *
 *   node src/03_shared.ts                       # and register it (see 01)
 *   curl localhost:8080/greeter/alice/greet
 *   curl localhost:8080/greeter/alice/history   # when alice was greeted, 02's greetings too
 */
import * as restate from 'restate-objects';

const greeter = restate.object({
  name: 'greeter',
  schema: ['CREATE TABLE greetings (at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)'], // 02's
  handlers: {
    greet: async (ctx: restate.ObjectContext) => {
      await ctx.db.execute('INSERT INTO greetings DEFAULT VALUES');
      return `Hello, ${ctx.key}!`;
    },

    history: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const rows = await ctx.db.query<{ at: string }>('SELECT at FROM greetings ORDER BY rowid');
      return rows.map(({ at }) => at);
    }),
  },
});

await restate.serve({ services: [greeter], port: 9080 });

/**
 * 03 · Reading beside the writer: views
 *
 * A key runs one handler at a time, so its writes never race. A view is the
 * exception: it runs whenever it is called, even while a handler of the key
 * runs, and reads the database as committed. It writes nothing.
 *
 *   node src/03_views.ts                        # and register it (see 01)
 *   curl localhost:8080/greeter/alice/greet
 *   curl localhost:8080/greeter/alice/history   # when alice was greeted, 02's greetings too
 */
import * as restate from 'restate-sessions';

const greeter = restate.sessionObject({
  name: 'greeter',
  schema: ['CREATE TABLE greetings (at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)'], // 02's
  handlers: {
    greet: async (ctx) => {
      await ctx.db.execute('INSERT INTO greetings DEFAULT VALUES');
      return `Hello, ${ctx.key}!`;
    },
  },
  views: {
    history: async (ctx) => {
      const rows = await ctx.db.query<{ at: string }>('SELECT at FROM greetings ORDER BY rowid');
      return rows.map(({ at }) => at);
    },
  },
});

await restate.serve({ services: [greeter], port: 9080 });

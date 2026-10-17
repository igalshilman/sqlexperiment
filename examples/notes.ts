/**
 * SQL in a virtual object: every key has a SQLite database of its own,
 * stored in Restate with the object's state. No database to run.
 *
 *   node notes.ts
 *   restate deployments register http://localhost:9080
 *   curl localhost:8080/notes/alice/add --json '"Buy milk"'
 *   curl localhost:8080/notes/alice/list
 */
import * as restate from '@restatedev/restate-sdk';

const SCHEMA = 'CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)';

const notes = restate.object({
  name: 'notes',
  handlers: {
    add: async (ctx: restate.SqlObjectContext, note: string) => {
      await ctx.db.execute(SCHEMA);
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },

    list: async (ctx: restate.SqlObjectContext) => {
      await ctx.db.execute(SCHEMA);
      const rows = await ctx.db.query('SELECT text FROM notes ORDER BY rowid');
      return rows.map(([text]) => text);
    },
  },
  options: restate.sqlOptions,
});

await restate.serve({ services: [notes] });

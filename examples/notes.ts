// SQL in a virtual object: each key has a database of its own, stored in
// Restate with the object's state.
//
//   node notes.ts
//   curl localhost:8080/notes/alice/add --json '"buy milk"'
//   curl localhost:8080/notes/alice/list --json '{}'
import * as restate from '@restatedev/restate-sdk';

const TABLE = 'CREATE TABLE IF NOT EXISTS notes (text TEXT)';

const notes = restate.object({
  name: 'notes',
  handlers: {
    add: async (ctx: restate.SqlObjectContext, note: string) => {
      await ctx.db.execute(TABLE);
      await ctx.db.execute('INSERT INTO notes VALUES (?)', [note]);
    },
    list: async (ctx: restate.SqlObjectContext) => {
      await ctx.db.execute(TABLE);
      return (await ctx.db.query('SELECT text FROM notes')).map(([text]) => text);
    },
  },
  options: restate.sqlOptions,
});

await restate.serve({ services: [notes] });

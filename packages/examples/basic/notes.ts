/**
 * SQL in a virtual object: every key has a SQLite database of its own,
 * stored in Restate with the object's state. No database to run.
 *
 *   node basic/notes.ts
 *   npx @restatedev/restate-server   # Restate, in another terminal
 *   npx @restatedev/restate deployments register http://localhost:9080
 *   curl localhost:8080/notes/alice/add --json '"Buy milk"'
 *   curl localhost:8080/notes/alice/list
 */
import * as restate from 'restate-objects';

const notes = restate.object({
  name: 'notes',
  // The schema of each key's database: its steps, applied once per key (a step added later runs then).
  schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
  handlers: {
    add: async (ctx: restate.ObjectContext, note: string) => {
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },

    /** Shared: it reads while `add` writes; a key with no notes yet has none. */
    list: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const rows = await ctx.db.values('SELECT text FROM notes ORDER BY rowid');
      return rows.map(([text]) => text);
    }),
  },
});

await restate.serve({ services: [notes], port: 9080 });

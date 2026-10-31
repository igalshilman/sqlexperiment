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
import * as restate from 'restate-sessions';

const notes = restate.sessionObject({
  name: 'notes',
  // Creates each key's database. Each statement runs once per key; append new ones later as needed.
  schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
  handlers: {
    add: async (ctx, note: string) => {
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },
  },
  views: {
    /** A view only reads, so it can run while `add` is writing. */
    list: async (ctx) => {
      const rows = await ctx.db.query<{ text: string }>('SELECT text FROM notes ORDER BY rowid');
      return rows.map(({ text }) => text);
    },
  },
});

await restate.serve({ services: [notes], port: 9080 });

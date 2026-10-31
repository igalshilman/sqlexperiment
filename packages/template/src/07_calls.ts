/**
 * 07 · Calling another object
 *
 * A handler calls another object's handler through Restate, and awaits its
 * answer. Each object keeps its own database: a nickname knows its owner, a
 * user knows their nicknames.
 *
 * A retried handler (06) makes its calls again. An idempotency key that is
 * the same on every attempt, the request's id and what the call is for,
 * makes Restate answer a repeated call with the first one's answer.
 *
 *   node src/07_calls.ts                                   # and register it (see 01)
 *   curl localhost:8080/user/alice/rename --json '"ace"'   # "alice is now ace"
 *   curl localhost:8080/user/bob/rename --json '"ace"'     # fails: ace is alice's
 */
import * as restate from 'restate-objects';

const nickname = restate.object({
  name: 'nickname',
  schema: ['CREATE TABLE owner (user TEXT NOT NULL)'],
  handlers: {
    /** The nickname's owner: `user`, unless someone claimed it first. */
    claim: async (ctx: restate.ObjectContext, user: string) => {
      await ctx.db.execute('INSERT INTO owner (user) SELECT ? WHERE NOT EXISTS (SELECT 1 FROM owner)', [user]);
      const [owner] = (await ctx.db.values('SELECT user FROM owner')).map(([name]) => String(name));
      return owner;
    },
  },
});

const user = restate.object({
  name: 'user',
  schema: ['CREATE TABLE nicknames (nickname TEXT NOT NULL)'],
  handlers: {
    rename: async (ctx: restate.ObjectContext, name: string) => {
      const owner = await ctx
        .objectClient(nickname, name)
        .claim(ctx.key, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:claim` }));
      if (owner !== ctx.key) throw new restate.TerminalError(`${name} is ${owner}'s`);
      await ctx.db.execute('INSERT INTO nicknames (nickname) VALUES (?)', [name]);
      return `${ctx.key} is now ${name}`;
    },
  },
});

await restate.serve({ services: [nickname, user], port: 9080 });

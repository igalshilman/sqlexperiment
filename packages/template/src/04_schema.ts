/**
 * 04 · Changing the schema
 *
 * Alice's database already exists, so editing its CREATE TABLE statement
 * would change nothing for her. That's why `schema` is a list of steps: to
 * change the tables, add a step at the end. Each key's database runs the
 * steps it hasn't run yet, the next time it is used. Never edit or remove a
 * step that may have run somewhere; add a new one instead.
 *
 * Here, deposits get a note.
 *
 *   node src/04_schema.ts                                                             # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json '{"amount": 50, "note": "birthday"}'
 *   curl localhost:8080/wallet/alice/history                                          # the earlier deposits have no note
 */
import * as restate from 'restate-sessions';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: [
    'CREATE TABLE entries (amount INTEGER NOT NULL)', // from lesson 02
    'ALTER TABLE entries ADD COLUMN note TEXT', // new: runs once on each wallet's database
  ],
  handlers: {
    deposit: async (ctx, { amount, note }: { amount: number; note?: string }) => {
      await ctx.db.execute('INSERT INTO entries (amount, note) VALUES (?, ?)', [amount, note ?? null]);
      return balanceOf(ctx.db);
    },
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),

    history: async (ctx) =>
      ctx.db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid'),
  },
});

await restate.serve({ services: [wallet], port: 9080 });

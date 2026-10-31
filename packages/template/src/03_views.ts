/**
 * 03 · Views
 *
 * Handlers of one key run one at a time, so their writes never race. The
 * price is that a slow handler makes every other request to that key wait.
 * Reading doesn't need to wait: a view runs as soon as it is called, even
 * while a handler of the same key is running, and reads what has been
 * committed so far. A view can't write.
 *
 *   node src/03_views.ts                               # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json 20
 *   curl localhost:8080/wallet/alice/balance           # 20, plus what lesson 02 deposited
 *   curl localhost:8080/wallet/alice/history           # every deposit, oldest first
 */
import * as restate from 'restate-sessions';

/** The sum of the entries. A `DatabaseReader` is any database you can read: a handler's, a view's or a transaction's. */
const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: ['CREATE TABLE entries (amount INTEGER NOT NULL)'],
  handlers: {
    deposit: async (ctx, amount: number) => {
      await ctx.db.execute('INSERT INTO entries (amount) VALUES (?)', [amount]);
      return balanceOf(ctx.db);
    },
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),

    history: async (ctx) => {
      const rows = await ctx.db.query<{ amount: number }>('SELECT amount FROM entries ORDER BY rowid');
      return rows.map(({ amount }) => amount);
    },
  },
});

await restate.serve({ services: [wallet], port: 9080 });

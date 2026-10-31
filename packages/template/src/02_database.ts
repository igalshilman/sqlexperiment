/**
 * 02 · A database per key
 *
 * Each key of a session object has its own SQLite database, `ctx.db`. It is
 * stored in Restate together with the object's state, so there is no
 * database server to run. `schema` lists the statements that create the
 * tables; they run once per key, the first time that key is used.
 *
 * The wallet records each deposit as a row, and the balance is their sum.
 * Bob's wallet is another key, so it has a database of its own.
 *
 *   node src/02_database.ts                              # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json 10   # 10
 *   curl localhost:8080/wallet/alice/deposit --json 5    # 15
 *   curl localhost:8080/wallet/bob/deposit --json 7      # 7
 */
import * as restate from 'restate-sessions';

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: ['CREATE TABLE entries (amount INTEGER NOT NULL)'],
  handlers: {
    deposit: async (ctx, amount: number) => {
      await ctx.db.execute('INSERT INTO entries (amount) VALUES (?)', [amount]);
      // `get` returns the first row as an object, with a property per column.
      const row = await ctx.db.get<{ balance: number }>('SELECT sum(amount) AS balance FROM entries');
      return row?.balance;
    },
  },
});

await restate.serve({ services: [wallet], port: 9080 });

/**
 * 05 · All or nothing: transactions
 *
 * Each statement on `ctx.db` commits on its own. `ctx.transaction` commits
 * several as one: all of them once its body returns, or none if it throws (an
 * SQL error, such as a failed CHECK, throws).
 *
 *   node src/05_transaction.ts                        # and register it (see 01)
 *   curl localhost:8080/wallet/alice/add --json 10    # 10
 *   curl localhost:8080/wallet/alice/add --json -25   # fails: the balance would go below zero
 *   curl localhost:8080/wallet/alice/history          # [10]: the failed change left no trace
 */
import * as restate from 'restate-objects';

const wallet = restate.object({
  name: 'wallet',
  schema: [
    'CREATE TABLE balance (amount INTEGER NOT NULL CHECK (amount >= 0))',
    'INSERT INTO balance VALUES (0)',
    'CREATE TABLE history (change INTEGER NOT NULL)',
  ],
  handlers: {
    /** Change the balance by `change` (negative to spend), and record the change: both, or neither. */
    add: (ctx: restate.ObjectContext, change: number) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('INSERT INTO history (change) VALUES (?)', [change]);
        // Below zero, this throws, and the insert above rolls back with it.
        await db.execute('UPDATE balance SET amount = amount + ?', [change]);
        const balance = await db.get<{ amount: number }>('SELECT amount FROM balance');
        return balance?.amount;
      }),

    history: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const rows = await ctx.db.query<{ change: number }>('SELECT change FROM history ORDER BY rowid');
      return rows.map(({ change }) => change);
    }),
  },
});

await restate.serve({ services: [wallet], port: 9080 });

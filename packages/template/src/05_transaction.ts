/**
 * 05 · Transactions
 *
 * Each statement on `ctx.db` commits on its own. When several changes must
 * happen together or not at all, put them in `ctx.db.transaction`: it
 * commits when its function returns, and rolls everything back if it throws.
 *
 * Spending records the payment first, then checks the balance. If the
 * balance went below zero, it throws, and the payment is rolled back as if
 * it never happened.
 *
 * It throws a `TerminalError`, which tells Restate not to retry: the request
 * fails, and the caller gets the error. Restate retries any other error,
 * which suits failures that may go away, such as a lost connection.
 *
 *   node src/05_transaction.ts                                                         # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json '{"amount": 30, "note": "salary"}'
 *   curl localhost:8080/wallet/alice/spend --json '{"amount": 12, "note": "lunch"}'      # the balance after lunch
 *   curl localhost:8080/wallet/alice/spend --json '{"amount": 1000, "note": "a boat"}'   # fails: not enough money
 *   curl localhost:8080/wallet/alice/history                                            # lunch, but no boat
 */
import * as restate from 'restate-sessions';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: ['CREATE TABLE entries (amount INTEGER NOT NULL)', 'ALTER TABLE entries ADD COLUMN note TEXT'],
  handlers: {
    deposit: async (ctx, { amount, note }: { amount: number; note?: string }) => {
      await ctx.db.execute('INSERT INTO entries (amount, note) VALUES (?, ?)', [amount, note ?? null]);
      return balanceOf(ctx.db);
    },

    spend: (ctx, { amount, note }: { amount: number; note?: string }) =>
      ctx.db.transaction(async (tx) => {
        await tx.execute('INSERT INTO entries (amount, note) VALUES (?, ?)', [-amount, note ?? null]);
        const balance = await balanceOf(tx);
        // Throwing rolls back the insert above.
        if (balance < 0) throw new restate.TerminalError('Not enough money');
        return balance;
      }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),

    history: async (ctx) =>
      ctx.db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid'),
  },
});

await restate.serve({ services: [wallet], port: 9080 });

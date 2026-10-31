/**
 * 08 · Sends that commit with the rows
 *
 * A send is a call that does not wait for its answer. A send made in a
 * transaction, with its `tx.send`, is part of it: Restate makes it once the
 * transaction commits, and never if it rolls back. Here, the money leaves
 * alice's account and is sent to bob's together, or not at all.
 *
 *   node src/08_sends.ts                                                               # and register it (see 01)
 *   curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'     # 100
 *   curl localhost:8080/account/alice/transfer --json '{"to": "bob", "amount": 30}'    # 70
 *   curl localhost:8080/account/bob/balance                                            # 30, once the send arrived
 *   curl localhost:8080/account/alice/transfer --json '{"to": "bob", "amount": 500}'   # fails, and sends nothing
 */
import * as restate from 'restate-sessions';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const account = restate.sessionObject({
  name: 'account',
  schema: ['CREATE TABLE entries (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)'],
  handlers: {
    /** Add `amount`, once per `id` (06). */
    deposit: async (ctx, { id, amount }: { id: string; amount: number }) => {
      await ctx.db.execute('INSERT OR IGNORE INTO entries (id, amount) VALUES (?, ?)', [id, amount]);
      return balanceOf(ctx.db);
    },

    /** Move `amount` to the account `to`, once per transfer (06). */
    transfer: (ctx, { to, amount }: { to: string; amount: number }) =>
      ctx.db.transaction(async (tx) => {
        const id = ctx.request.id; // the transfer's id: the same on every attempt
        // A retry finds the transfer committed, and its send made: it moves nothing.
        const done = await tx.get('SELECT 1 FROM entries WHERE id = ?', [id]);
        if (done === undefined) {
          if ((await balanceOf(tx)) < amount) throw new restate.TerminalError('Insufficient funds');
          await tx.execute('INSERT INTO entries (id, amount) VALUES (?, ?)', [id, -amount]);
          tx.send(account, to).deposit({ id, amount }); // made once this commits
        }
        return balanceOf(tx);
      }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
  },
});

await restate.serve({ services: [account], port: 9080 });

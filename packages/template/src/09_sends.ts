/**
 * 09 · Sends that commit with the rows
 *
 * A send is a call that does not wait for its answer. A send made in a
 * transaction, with its `tx`, is part of it: Restate makes it once the
 * transaction commits, and never if it rolls back. Here, the money leaves
 * alice's account and is sent to bob's together, or not at all.
 *
 *   node src/09_sends.ts                                                               # and register it (see 01)
 *   curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'     # 100
 *   curl localhost:8080/account/alice/transfer --json '{"to": "bob", "amount": 30}'    # 70
 *   curl localhost:8080/account/bob/balance                                            # 30, once the send arrived
 *   curl localhost:8080/account/alice/transfer --json '{"to": "bob", "amount": 500}'   # fails, and sends nothing
 */
import * as restate from 'restate-objects';

const balanceOf = async (db: restate.SqlDatabase) => {
  const [balance] = (await db.query('SELECT coalesce(sum(amount), 0) FROM entries')).map(([sum]) => sum);
  return Number(balance);
};

const account = restate.object({
  name: 'account',
  schema: ['CREATE TABLE entries (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)'],
  handlers: {
    /** Add `amount`, once per `id` (06). */
    deposit: async (ctx: restate.ObjectContext, { id, amount }: { id: string; amount: number }) => {
      await ctx.db.execute('INSERT OR IGNORE INTO entries (id, amount) VALUES (?, ?)', [id, amount]);
      return balanceOf(ctx.db);
    },

    /** Move `amount` to the account `to`: a step (07), so a retry does not move it twice. */
    transfer: (ctx: restate.ObjectContext, { to, amount }: { to: string; amount: number }) =>
      ctx.step('transfer', async ({ db, tx }) => {
        if ((await balanceOf(db)) < amount) throw new restate.TerminalError('Insufficient funds');
        const id = ctx.request().id; // the transfer's id: the same on every attempt
        await db.execute('INSERT INTO entries (id, amount) VALUES (?, ?)', [id, -amount]);
        tx.objectSendClient(account, to).deposit({ id, amount }); // made once this commits
        return balanceOf(db);
      }),

    balance: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => balanceOf(ctx.db)),
  },
});

await restate.serve({ services: [account], port: 9080 });

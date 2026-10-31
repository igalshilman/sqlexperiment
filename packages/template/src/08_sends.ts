/**
 * 08 · Sends that commit with the transaction
 *
 * A transfer takes money out of alice's wallet and puts it into bob's.
 * Alice's wallet doesn't need an answer from bob's, so instead of calling it
 * (07), it sends a message, which doesn't wait. Waiting could even get stuck:
 * if bob paid alice at the same moment, each wallet would wait for the
 * other, since each runs one request at a time.
 *
 * A send made with a transaction's `tx.send` belongs to that transaction:
 * Restate delivers it once the transaction commits, and never if it rolls
 * back. So the money leaves alice and arrives at bob together, or not at
 * all.
 *
 *   node src/08_sends.ts                                                                # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json '{"id": "d2", "amount": 100}'
 *   curl localhost:8080/wallet/alice/transfer --json '{"to": "bob", "amount": 30}'      # alice's balance after the transfer
 *   curl localhost:8080/wallet/bob/history                                              # 30, from alice
 *   curl localhost:8080/wallet/alice/transfer --json '{"to": "bob", "amount": 5000}'    # fails, and bob gets nothing
 */
import * as restate from 'restate-sessions';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: [
    'CREATE TABLE entries (amount INTEGER NOT NULL)',
    'ALTER TABLE entries ADD COLUMN note TEXT',
    'ALTER TABLE entries ADD COLUMN id TEXT',
    'CREATE UNIQUE INDEX entries_by_id ON entries (id)',
  ],
  handlers: {
    deposit: async (ctx, { id, amount, note }: { id: string; amount: number; note?: string }) => {
      await ctx.db.execute('INSERT OR IGNORE INTO entries (id, amount, note) VALUES (?, ?, ?)', [
        id,
        amount,
        note ?? null,
      ]);
      return balanceOf(ctx.db);
    },

    transfer: (ctx, { to, amount }: { to: string; amount: number }) =>
      ctx.db.transaction(async (tx) => {
        // The transfer's id, the same on every attempt (06). Bob's wallet records it too.
        const id = ctx.request.id;
        // A retry finds the transfer already committed, with its send: it does nothing more.
        const done = await tx.get('SELECT 1 FROM entries WHERE id = ?', [id]);
        if (done === undefined) {
          await tx.execute('INSERT INTO entries (id, amount, note) VALUES (?, ?, ?)', [id, -amount, `to ${to}`]);
          if ((await balanceOf(tx)) < 0) throw new restate.TerminalError('Not enough money');
          tx.send(wallet, to).deposit({ id, amount, note: `from ${ctx.key}` }); // delivered once this commits
        }
        return balanceOf(tx);
      }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),

    history: async (ctx) =>
      ctx.db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid'),
  },
});

await restate.serve({ services: [wallet], port: 9080 });

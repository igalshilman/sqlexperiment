/**
 * 07 · Calling another object
 *
 * Friends should be able to pay alice without knowing her wallet's key, so
 * she picks a handle, such as @ace. Handles are a second session object:
 * each handle is a key with its own database, which records the wallet that
 * owns it. The first wallet to claim a handle gets it.
 *
 * `ctx.rpc(handle, name)` calls that object through Restate, and waits for
 * the answer. A retried handler (06) makes its calls again, so a call that
 * changes something gets an idempotency key that is the same on every
 * attempt. Restate then answers a repeated call with the first call's
 * result, instead of running it again.
 *
 *   node src/07_calls.ts                                    # and register it (see 01)
 *   curl localhost:8080/wallet/alice/claim --json '"ace"'   # "alice is now @ace"
 *   curl localhost:8080/wallet/bob/claim --json '"ace"'     # fails: @ace belongs to alice
 *   curl localhost:8080/handle/ace/owner                    # "alice"
 */
import * as restate from 'restate-sessions';

const handle = restate.sessionObject({
  name: 'handle',
  schema: ['CREATE TABLE owner (wallet TEXT NOT NULL)'],
  handlers: {
    /** Give the handle to `wallet`, unless another wallet has it, and answer with its owner. */
    claim: async (ctx, wallet: string) => {
      await ctx.db.execute('INSERT INTO owner (wallet) SELECT ? WHERE NOT EXISTS (SELECT 1 FROM owner)', [wallet]);
      const owner = await ctx.db.get<{ wallet: string }>('SELECT wallet FROM owner');
      return owner!.wallet; // there is one: the insert above made sure
    },
  },
  views: {
    owner: async (ctx) => (await ctx.db.get<{ wallet: string }>('SELECT wallet FROM owner'))?.wallet ?? null,
  },
});

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

    claim: async (ctx, name: string) => {
      const options = { idempotencyKey: `${ctx.request.id}:claim` };
      const owner = await ctx.rpc(handle, name, options).claim(ctx.key);
      if (owner !== ctx.key) throw new restate.TerminalError(`@${name} belongs to ${owner}`);
      return `${ctx.key} is now @${name}`;
    },
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
  },
});

await restate.serve({ services: [wallet, handle], port: 9080 });

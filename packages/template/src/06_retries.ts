/**
 * 06 · Retries
 *
 * When a handler fails partway, because the process crashed or a connection
 * dropped, Restate runs it again from the first line. Whatever the failed
 * attempt committed is still in the database, so every handler must be safe
 * to run twice.
 *
 * The way to do that is an id that is the same on every attempt. A deposit
 * now comes with an id from the caller, and `INSERT OR IGNORE` skips a
 * deposit whose id is already there: running it twice, or sending it twice,
 * counts it once. Spending has no id from the caller, so it uses
 * `ctx.request.id`, the id Restate gives each request, which also stays the
 * same across attempts.
 *
 * To show it, this lesson fails the first attempt of every deposit right
 * after the insert, the way a crash would. Restate retries it, and the
 * balance still counts the deposit once.
 *
 *   node src/06_retries.ts                                                                         # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json '{"id": "d1", "amount": 10, "note": "refund"}'   # counted once, though the first attempt failed
 *   curl localhost:8080/wallet/alice/deposit --json '{"id": "d1", "amount": 10, "note": "refund"}'   # sent again: the balance stays the same
 */
import * as restate from 'restate-sessions';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const failed = new Set<string>();

/** Fail the first attempt of a request, as a crash would. Restate then retries it. */
function failFirstAttempt(ctx: restate.SessionContext) {
  if (failed.has(ctx.request.id)) return;
  failed.add(ctx.request.id);
  throw new Error('Simulated crash, after the deposit was committed');
}

const wallet = restate.sessionObject({
  name: 'wallet',
  schema: [
    'CREATE TABLE entries (amount INTEGER NOT NULL)',
    'ALTER TABLE entries ADD COLUMN note TEXT',
    // New: an id per entry. Entries from earlier lessons have none, which a unique index allows.
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
      failFirstAttempt(ctx);
      return balanceOf(ctx.db);
    },

    spend: (ctx, { amount, note }: { amount: number; note?: string }) =>
      ctx.db.transaction(async (tx) => {
        await tx.execute('INSERT OR IGNORE INTO entries (id, amount, note) VALUES (?, ?, ?)', [
          ctx.request.id,
          -amount,
          note ?? null,
        ]);
        const balance = await balanceOf(tx);
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

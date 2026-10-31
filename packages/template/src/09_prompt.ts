/**
 * 09 · A model in a handler
 *
 * From here on, the wallet talks to a model, so it is defined with `object`
 * from restate-agents. That is `restate.sessionObject` with what agents need
 * set up: their tables in the same database, and a few built-in handlers you
 * will meet in the next lessons.
 *
 * `pi.prompt(ctx, text)` asks a model from a handler and returns the answer.
 * The prompt and the answer are stored in the wallet's database, so if
 * Restate retries the handler, the model isn't asked a second time.
 *
 *   OPENAI_API_KEY=… node src/09_prompt.ts      # and register it (see 01)
 *   curl localhost:8080/wallet/alice/summary    # a few words on alice's money
 */
import type * as restate from 'restate-sessions';
import { object, serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const historyOf = (db: restate.DatabaseReader) =>
  db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid');

const wallet = object({
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

    summary: async (ctx) => {
      const entries = JSON.stringify(await historyOf(ctx.db));
      return pi.prompt(
        ctx,
        `These are the entries of a wallet, oldest first; negative amounts are spending.\n${entries}\n` +
          'Describe them to the owner in two short sentences.',
      );
    },
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    history: async (ctx) => historyOf(ctx.db),
  },
});

await serve(wallet);

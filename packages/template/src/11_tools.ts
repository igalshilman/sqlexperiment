/**
 * 11 · Tools
 *
 * A tool is a function the model can decide to call. It has a name and a
 * description, which tell the model what it is for, its input as a zod
 * schema, and `run`. `run` gets the wallet's `ctx`, so it reads the same
 * database the conversation is stored in.
 *
 * When a tool throws a `TerminalError`, the model gets the error message as
 * the tool's answer, and can correct itself. Restate retries any other error,
 * as in any handler.
 *
 *   OPENAI_API_KEY=… node src/11_tools.ts   # and register it (see 01)
 *   curl localhost:8080/wallet/alice/chat -d 'How much do I have, and what did I spend money on?'
 */
import * as restate from 'restate-sessions';
import { sessionObject, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const historyOf = (db: restate.DatabaseReader) =>
  db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid');

const balance = tool({
  name: 'balance',
  description: 'The current balance of the wallet.',
  input: z.object({}),
  run: (_input, { ctx }) => balanceOf(ctx.db),
});

const history = tool({
  name: 'history',
  description: 'The latest entries of the wallet, newest first. Negative amounts are spending.',
  input: z.object({ count: z.number().describe('How many entries, at most 100.') }),
  run: async ({ count }, { ctx }) => {
    if (count > 100) throw new restate.TerminalError('At most 100 entries at a time');
    return (await historyOf(ctx.db)).toReversed().slice(0, count);
  },
});

const wallet = sessionObject({
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

    chat: pi.chat({
      system: "You are the assistant of the user's wallet. Keep your answers short.",
      tools: [balance, history],
    }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet);

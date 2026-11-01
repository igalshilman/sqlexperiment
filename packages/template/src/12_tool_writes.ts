/**
 * 12 · A tool that moves money
 *
 * Tools can write too. The `transfer` tool is lesson 08's transfer, taken
 * out of the handler into a function. Like any handler, `chat` may be
 * retried, and a crash in the middle of a tool call runs that tool call
 * again. So a tool that changes something needs an id that is the same each
 * time it runs, just like a handler (06). `run` gets one: `idempotencyKey`,
 * made of the request's id and the model's id for the tool call.
 *
 *   OPENAI_API_KEY=… node src/12_tool_writes.ts                         # and register it (see 01)
 *   curl localhost:8080/wallet/alice/chat -d 'Send 20 to bob for the pizza.'
 *   curl localhost:8080/wallet/bob/history                              # 20, from alice
 *   curl localhost:8080/wallet/alice/chat -d 'Now send him a million.'  # the tool answers "Not enough money"
 */
import * as restate from 'restate-sessions';
import { sessionObject, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const historyOf = (db: restate.DatabaseReader) =>
  db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid');

/** Move `amount` to the wallet `to`, once per `id`, and return the balance left (08). */
function transfer(ctx: restate.SessionContext, id: string, to: string, amount: number): Promise<number> {
  return ctx.db.transaction(async (tx) => {
    const done = await tx.get('SELECT 1 FROM entries WHERE id = ?', [id]);
    if (done === undefined) {
      await tx.execute('INSERT INTO entries (id, amount, note) VALUES (?, ?, ?)', [id, -amount, `to ${to}`]);
      if ((await balanceOf(tx)) < 0) throw new restate.TerminalError('Not enough money');
      tx.send(wallet, to).deposit({ id, amount, note: `from ${ctx.key}` });
    }
    return balanceOf(tx);
  });
}

const send = tool({
  name: 'transfer',
  description: "Send money from the user's wallet to another wallet, by its owner's name.",
  input: z.object({ to: z.string(), amount: z.number().int().positive() }),
  run: async ({ to, amount }, { ctx, idempotencyKey }) => {
    const left = await transfer(ctx, idempotencyKey, to, amount);
    return `Sent ${amount} to ${to}. The balance is now ${left}.`;
  },
});

const balance = tool({
  name: 'balance',
  description: 'The current balance of the wallet.',
  input: z.object({}),
  run: (_input, { ctx }) => balanceOf(ctx.db),
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
      tools: [balance, send],
    }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    history: async (ctx) => historyOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet);

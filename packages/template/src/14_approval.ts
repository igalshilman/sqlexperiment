/**
 * 14 · A person in the loop
 *
 * Large transfers should wait until the owner confirms them in their app. A
 * `signal` tool waits for a value from outside, for as long as it takes: no
 * process is held while it waits, and a crash or a restart changes nothing.
 * `pending` lists what the agent is waiting for, and `signal` answers it.
 * Both are views every agent has.
 *
 * Here the model asks for the confirmation because the tools' descriptions
 * tell it to.
 *
 *   OPENAI_API_KEY=… node src/14_approval.ts                                     # and register it (see 01)
 *   curl localhost:8080/wallet/alice/deposit --json '{"id": "d3", "amount": 200, "note": "salary"}'
 *   curl localhost:8080/wallet/alice/chat/send -d 'Send 150 to bob for the rent.'
 *   curl localhost:8080/wallet/alice/pending                                     # the confirmation it waits for
 *   curl localhost:8080/wallet/alice/signal --json '{"name": "confirmation", "value": "confirmed"}'
 *   curl localhost:8080/wallet/alice/messages                                    # the transfer, and the answer
 *
 * `/send` sends the message without waiting for the answer, which waits for
 * the confirmation.
 */
import * as restate from 'restate-sessions';
import { sessionObject, serve, signal, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const historyOf = (db: restate.DatabaseReader) =>
  db.query<{ amount: number; note: string | null }>('SELECT amount, note FROM entries ORDER BY rowid');

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
  description:
    "Send money from the user's wallet to another wallet, by its owner's name. " +
    'A transfer over 100 needs a confirmation first.',
  input: z.object({ to: z.string(), amount: z.number().int().positive() }),
  run: async ({ to, amount }, { ctx, idempotencyKey }) => {
    const left = await transfer(ctx, idempotencyKey, to, amount);
    return `Sent ${amount} to ${to}. The balance is now ${left}.`;
  },
});

const confirmation = signal('confirmation', {
  description:
    'Ask the owner to confirm a transfer over 100 in their app, and wait for their answer: "confirmed" or "declined".',
  input: z.object({ to: z.string(), amount: z.number() }),
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
      tools: [confirmation, send],
    }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    history: async (ctx) => historyOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet);

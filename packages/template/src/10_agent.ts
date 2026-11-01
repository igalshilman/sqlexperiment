/**
 * 10 · An agent
 *
 * `pi.chat()` is a handler that answers a message, and `pi.messages()` is a
 * view of the conversation so far. Add them to the wallet, and every wallet
 * has an assistant whose conversation is stored in that wallet's database,
 * next to its entries. Like any handler, `chat` answers one message per
 * wallet at a time; `messages` is a view, so it answers even while the
 * assistant is busy.
 *
 * The assistant can't see the wallet yet: that is the next lesson.
 *
 *   OPENAI_API_KEY=… node src/10_agent.ts                                # and register it (see 01)
 *   curl localhost:8080/wallet/alice/chat -d 'Hi! I am saving up for a bike.'
 *   curl localhost:8080/wallet/alice/chat -d 'What am I saving for?'     # it remembers
 *   curl localhost:8080/wallet/bob/chat -d 'What am I saving for?'       # bob's wallet has a conversation of its own
 *   curl localhost:8080/wallet/alice/messages                            # alice's conversation, 09's summaries included
 */
import type * as restate from 'restate-sessions';
import { sessionObject, serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

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

    chat: pi.chat({ system: "You are the assistant of the user's wallet. Keep your answers short." }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet);

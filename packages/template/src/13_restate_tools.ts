/**
 * 13 · Handlers as tools
 *
 * A handler or a view of any Restate object can be a tool, without writing
 * `run`: `call(handle, 'owner', { description })` lets the model look up who
 * owns a handle (07). The model picks the key, here the handle's name, and
 * the agent calls it through Restate: once, however often `chat` is retried.
 *
 * With it, alice can pay friends by their handle.
 *
 *   OPENAI_API_KEY=… node src/13_restate_tools.ts                       # and register it (see 01)
 *   curl localhost:8080/handle/bobby/claim --json '"bob"'               # bob claims @bobby
 *   curl localhost:8080/wallet/alice/chat -d 'Send 15 to @bobby for the tickets.'
 *   curl localhost:8080/wallet/alice/messages                           # the lookup, then the transfer
 */
import * as restate from 'restate-sessions';
import { call, object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const handle = restate.sessionObject({
  name: 'handle',
  schema: ['CREATE TABLE owner (wallet TEXT NOT NULL)'],
  handlers: {
    claim: async (ctx, wallet: string) => {
      await ctx.db.execute('INSERT INTO owner (wallet) SELECT ? WHERE NOT EXISTS (SELECT 1 FROM owner)', [wallet]);
      return (await ctx.db.get<{ wallet: string }>('SELECT wallet FROM owner'))!.wallet;
    },
  },
  views: {
    owner: async (ctx) => (await ctx.db.get<{ wallet: string }>('SELECT wallet FROM owner'))?.wallet ?? null,
  },
});

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
  description: "Send money from the user's wallet to another wallet, by its owner's name (not a handle).",
  input: z.object({ to: z.string(), amount: z.number().int().positive() }),
  run: async ({ to, amount }, { ctx, idempotencyKey }) => {
    const left = await transfer(ctx, idempotencyKey, to, amount);
    return `Sent ${amount} to ${to}. The balance is now ${left}.`;
  },
});

const owner = call(handle, 'owner', {
  description: 'The owner of a handle: for @bobby, call it with the key "bobby". Null if nobody has it.',
});

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

    chat: pi.chat({
      system: "You are the assistant of the user's wallet. Keep your answers short.",
      tools: [owner, send],
    }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    history: async (ctx) => historyOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet, handle);

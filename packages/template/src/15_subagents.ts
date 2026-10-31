/**
 * 15 · Agents that delegate
 *
 * `subagent(coach)` makes another agent a tool. The wallet's assistant hands
 * the coach a task with a name, and gets the coach's answer. Each task is a
 * conversation of the coach of its own, keyed by the wallet and the task's
 * name (such as `coach/alice.bike`), so the assistant can come back to it
 * later, and a crash anywhere resumes where it stopped.
 *
 *   OPENAI_API_KEY=… node src/15_subagents.ts                           # and register it (see 01)
 *   curl localhost:8080/wallet/alice/chat -d 'I want the bike, 600, by June. Ask the coach for a plan.'
 *   curl localhost:8080/wallet/alice/messages                           # the task it gave the coach, and the plan
 */
import type * as restate from 'restate-sessions';
import { object, serve, subagent, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

const balance = tool({
  name: 'balance',
  description: 'The current balance of the wallet.',
  input: z.object({}),
  run: (_input, { ctx }) => balanceOf(ctx.db),
});

const coach = pi.agent({
  name: 'coach',
  description: 'A savings coach. Give it a goal, a deadline and the current balance, and it answers with a plan.',
  system: 'You are a savings coach. Answer with a short, realistic plan: how much to put aside each week, and one tip.',
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
      tools: [balance, subagent(coach)],
    }),
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    messages: pi.messages(),
  },
});

await serve(wallet, coach);

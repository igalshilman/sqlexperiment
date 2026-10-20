// A tool that writes to the agent's own database (`ctx.db`) during a pi turn,
// as examples/memory.ts does, without a Restate server: the facts and the
// conversation share the object's database, and a retried attempt stores a
// fact once.
import type { ObjectContext as SqlObjectContext } from 'restate-sql';
import { tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { describe, expect, it, vi } from 'vitest';
import { z } from 'zod';
import { callTools, fakeProvider, say, textOf } from '../fakes/pi.ts';
import { sqlAttempt } from '../testing.ts';

// Told "remember X", it calls remember; asked what it knows, it calls recall;
// given a tool's result, it says it.
vi.mock('@earendil-works/pi-ai/providers/anthropic', () => ({
  anthropicProvider: () =>
    fakeProvider('anthropic', 'claude-sonnet-5-5', ({ messages }) => {
      const last = messages.at(-1)!;
      if (last.role === 'toolResult') return say(textOf(last.content));
      const text = textOf(last.content);
      if (text.startsWith('remember ')) return callTools([['remember', { fact: text.slice('remember '.length) }]]);
      return callTools([['recall', {}]]);
    }),
}));

const SCHEMA = 'CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute(SCHEMA);
    await ctx.db.execute('INSERT OR IGNORE INTO facts (fact) VALUES (?)', [fact]);
    return `Remembered: ${fact}`;
  },
});

const recall = tool({
  name: 'recall',
  description: 'Everything remembered.',
  input: z.object({}),
  run: async (_input, { ctx }) => {
    await ctx.db.execute(SCHEMA);
    return (await ctx.db.query('SELECT fact FROM facts ORDER BY rowid')).map(([fact]) => fact);
  },
});

type Handler = (ctx: SqlObjectContext, input: string) => Promise<unknown>;

/** The object with key `key`: its database lives across invocations. */
function store(key: string) {
  const map = new Map<string, Uint8Array>();
  return (handler: Handler, id: string, input = '') => {
    const sql = sqlAttempt({ map, key, id });
    const ctx = { key, ...sql };
    return sql.end(() => handler(ctx as unknown as SqlObjectContext, input));
  };
}

const chat = pi.chat({ tools: [remember, recall] }) as Handler;
const facts: Handler = async (ctx) => {
  await ctx.db.execute(SCHEMA);
  return (await ctx.db.query('SELECT fact FROM facts ORDER BY rowid')).map(([fact]) => fact);
};

describe('a tool on the agent database', () => {
  it('stores facts beside the conversation, and reads them back in a later turn', async () => {
    const alice = store('alice');
    expect(await alice(chat, 'inv1', 'remember I am vegetarian')).toBe('Remembered: I am vegetarian');
    expect(await alice(chat, 'inv2', 'remember I live in Lisbon')).toBe('Remembered: I live in Lisbon');
    // Retried, an answered message is answered again, and stores nothing more.
    expect(await alice(chat, 'inv2', 'remember I live in Lisbon')).toBe('Remembered: I live in Lisbon');
    expect(await alice(chat, 'inv3', 'what do you know?')).toBe('["I am vegetarian","I live in Lisbon"]');
    expect(await alice(facts, 'read')).toEqual(['I am vegetarian', 'I live in Lisbon']);
  });
});

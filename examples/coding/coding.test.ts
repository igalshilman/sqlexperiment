// The coding team without a Restate server or an API key: the coordinator's
// model (Anthropic's provider) is a fake that hands out two tasks, and the
// coders' OpenCode is a stub that reports where it worked.
import type * as restate from '@restatedev/restate-sdk';
import { describe, expect, it, vi } from 'vitest';
import { sqlAttempt } from '../testing.ts';
import { callTools, fakeProvider, say, textOf } from '../testing/pi.ts';
import { coder, coordinator } from './coding.ts';

vi.mock('@earendil-works/pi-ai/providers/anthropic', () => ({
  anthropicProvider: () =>
    fakeProvider('anthropic', 'claude-sonnet-5-5', ({ messages }) => {
      const last = messages.at(-1)!;
      if (last.role === 'toolResult') {
        const results = messages.filter((m) => m.role === 'toolResult').map((m) => textOf(m.content));
        return say(results.join('\n'));
      }
      return callTools([
        ['code', { name: 'fizzbuzz', task: 'write fizzbuzz.py' }],
        ['code', { name: 'primes', task: 'write primes.py' }],
      ]);
    }),
}));

vi.mock('../opencode/with-opencode.ts', () => ({
  prompt: async (_ctx: unknown, task: string, { directory }: { directory: string }) => `did "${task}" in ${directory}`,
}));

type Handler = (ctx: restate.SqlObjectContext, text: string) => Promise<string>;
const handlers = (o: unknown) => (o as { object: Record<string, Handler> }).object;

describe('the coordinator', () => {
  it('hands each task to the coder of its name, once per tool call', async () => {
    const calls: { key: string; task: string; idempotencyKey?: string }[] = [];
    const sql = sqlAttempt({ map: new Map(), key: 'alice', id: 'inv1' });
    const ctx = {
      key: 'alice',
      request: () => ({ id: 'inv1' }),
      db: sql.db,
      transaction: sql.transaction,
      objectClient: (_object: unknown, key: string) => ({
        code: async (task: string, opts: restate.Opts<string, string>) => {
          calls.push({ key, task, idempotencyKey: opts.getOpts().idempotencyKey });
          return `${key}: done`;
        },
      }),
    };
    const answer = await sql.end(() =>
      handlers(coordinator).chat!(ctx as unknown as restate.SqlObjectContext, 'fizzbuzz and primes, please'),
    );
    expect(answer).toBe('alice.fizzbuzz: done\nalice.primes: done');
    expect(calls.map((c) => [c.key, c.task])).toEqual([
      ['alice.fizzbuzz', 'write fizzbuzz.py'],
      ['alice.primes', 'write primes.py'],
    ]);
    // One idempotency key per tool call, made from the invocation id.
    expect(new Set(calls.map((c) => c.idempotencyKey)).size).toBe(2);
    expect(calls.every((c) => c.idempotencyKey?.startsWith('inv1:'))).toBe(true);
  });
});

describe('a coder', () => {
  it('works in a directory of its key, in WORKSPACE', async () => {
    vi.stubEnv('WORKSPACE', '/work');
    try {
      const ctx = { key: 'alice.fizzbuzz' } as restate.SqlObjectContext;
      expect(await handlers(coder).code!(ctx, 'write fizzbuzz.py')).toBe(
        'did "write fizzbuzz.py" in /work/alice.fizzbuzz',
      );
    } finally {
      vi.unstubAllEnvs();
    }
  });
});

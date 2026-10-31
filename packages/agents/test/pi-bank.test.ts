// The bank agent on pi (./pi.ts) without a Restate server, through crashes of
// the handler (see ../testing.ts). Its model is the teller (../fakes/pi.ts).
// `pnpm --filter restate-agents test pi`.
import { describe, expect, it, vi } from 'vitest';
import { conversation, crashedAnywhere, handlerOf, SAID } from './support.ts';
import type * as restate from 'restate-objects';
import * as pi from 'restate-agents/pi';
import { prompt } from './pi.ts';

// The default model's provider, OpenAI's, is the teller.
vi.mock('@earendil-works/pi-ai/providers/all', async () => {
  const { fakeProvider, teller } = await import('./fakes/pi.ts');
  return { builtinProviders: () => [fakeProvider('openai', 'gpt-6-sol', teller)] };
});

/** The bank agent, after making a table of yours named as one of pi-durable's. */
const besideYours = async (ctx: restate.SessionContext, message: string) => {
  await ctx.db.execute('CREATE TABLE IF NOT EXISTS tasks (task TEXT)');
  return handlerOf(prompt)(ctx, message);
};

describe('the bank agent on pi', () => {
  it('answers a message once after a crash at any of its state requests', async () => {
    // One at a time: pi-ai's faux provider answers from one queue.
    expect(await crashedAnywhere(prompt, pi.messages(), { concurrency: 1 })).toEqual([]);
  }, 600_000);

  it('answers a message once after a crash at any of its state requests, steered once', async () => {
    // One at a time: pi-ai's faux provider answers from one queue.
    expect(await crashedAnywhere(prompt, pi.messages(), { concurrency: 1, steered: true })).toEqual([]);
  }, 600_000);

  it('runs a tool call again however often a crash cuts it off', async () => {
    // pi-durable parks a task cut off 5 times in a row by default: Restate's retries decide here.
    const c = conversation(prompt, 'restarts');
    for (let crash = 0; crash < 8; crash++) await c.crash('inv1', 'Please move 5 from 1 to 2');
    expect(await c.attempt('inv1', 'Please move 5 from 1 to 2')).toMatch(/^Done: moved 5/);
    expect(c.ledger.transfers.size).toBe(1);
  }, 120_000);

  it('tells the model a tool it does not have is not there', async () => {
    // The teller audits with a tool `audit`, which this agent lacks.
    const c = conversation(pi.chat(), 'unknown');
    expect(await c.attempt('inv1', 'audit the books')).toMatch(/^Done: .*audit/s);
  });

  it('keeps its tables apart from yours: each is restate_pi_<name>', async () => {
    const c = conversation(besideYours, 'tables');
    expect(await c.attempt('inv1', 'Please move 5 from 1 to 2')).toMatch(/^Done: /);
    const names = await c.read(async (ctx) =>
      (await ctx.db.values("SELECT name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")).map(([name]) => name),
    );
    expect(names.filter((name) => !String(name).startsWith('restate_'))).toEqual(['tasks']);
  });

  it('gives the conversation: what was said, in order, with the tool calls', async () => {
    const c = conversation(prompt, 'm');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    expect(await c.read(pi.messages(), {})).toEqual({ messages: SAID, more: false, cursor: expect.any(Number) });
  });
});

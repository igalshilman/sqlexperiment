// The bank agent on pi (./pi.ts) without a Restate server, through crashes of
// the handler (see ../testing.ts). Its model is the teller (../fakes/pi.ts).
// `make test-ts T=pi`.
import { describe, expect, it, vi } from 'vitest';
import { conversation, crashedAnywhere, SAID } from '../testing.ts';
import * as pi from 'restate-agents/pi';
import { prompt } from './pi.ts';

// The default model's provider, Anthropic's, is the teller.
vi.mock('@earendil-works/pi-ai/providers/anthropic', async () => {
  const { fakeProvider, teller } = await import('../fakes/pi.ts');
  return { anthropicProvider: () => fakeProvider('anthropic', 'claude-sonnet-5-5', teller) };
});

describe('the bank agent on pi', () => {
  it('answers a message once after a crash at any of its state requests', async () => {
    // One at a time: pi-ai's faux provider answers from one queue.
    expect(await crashedAnywhere(prompt, pi.messages(), { concurrency: 1 })).toEqual([]);
  }, 600_000);

  it('gives the conversation: what was said, in order, with the tool calls', async () => {
    const c = conversation(prompt, 'm');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    expect(await c.read(pi.messages(), {})).toEqual({ messages: SAID, more: false });
  });
});

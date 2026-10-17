// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): a crashed attempt gets no chance to store
// anything on the way out. Its model is the teller (../testing/opencode.ts).
// `npm run test:opencode` (or make test-opencode).
import { describe, expect, it, vi } from 'vitest';
import { conversation } from '../testing.ts';
import { teller, withTeller } from '../testing/opencode.ts';
import { prompt } from './agent.ts';
import type * as OpenCodeDb from './opencode-db.ts';

vi.mock('./opencode-db.ts', async (original) => {
  const db = await original<typeof OpenCodeDb>();
  return { ...db, openOpenCode: withTeller(db.openOpenCode) };
});
vi.stubEnv('OPENCODE_MODEL', 'teller/teller');

describe('the OpenCode agent', () => {
  it('answers a prompt once, by its id', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', { id: 'p1', text: 'Please move 5 from 1 to 2' });
    expect(first).toEqual({ answer: 'Done: moved 5 from account 1 to account 2' });
    // Asked again, in a new invocation: the same answer, no new transfer.
    expect(await c.attempt('inv2', { id: 'p1', text: 'Please move 5 from 1 to 2' })).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect([...c.ledger.transfers.keys()]).toEqual([expect.stringMatching(/^OpenCodeAgent\/a\/call_/)]);
    // The next prompt goes on in the same session.
    const audit = await c.attempt('inv3', { id: 'p2', text: 'audit the books' });
    expect(audit.answer).toMatch(/^Done: .*"transfers":1/);
  });

  it('resumes a crashed attempt, retrying the interrupted transfer', async () => {
    const c = conversation(prompt, 'b');
    const calls = { model: 0 };
    const reply = teller.reply;
    teller.reply = (messages, tools) => {
      if (tools.length > 0) calls.model++;
      return reply(messages, tools);
    };
    try {
      const input = { id: 'p1', text: 'Please move 7 from 3 to 4' };
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt resumes the turn: the model retries the
      // interrupted transfer, then answers.
      expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 7 from account 3 to account 4' });
      expect(calls.model).toBe(3);
    } finally {
      teller.reply = reply;
    }
    // The same transfer id and idempotency key both times: the money moved once.
    const [first, retry] = c.ledger.calls;
    expect(c.ledger.calls).toHaveLength(2);
    expect(retry).toEqual(first);
    expect(first?.key).toBe(`inv1:${first?.id.split('/').at(-1)}`);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it("fails the attempt on a tool's error, rather than hand it to the model", async () => {
    const c = conversation(prompt, 'd');
    c.ledger.failNext = 1;
    const input = { id: 'p1', text: 'Please move 3 from 5 to 6' };
    await expect(c.attempt('inv1', input)).rejects.toThrow('crash after the transfer');
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 3 from account 5 to account 6' });
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('resumes a turn however many attempts crash: Restate decides', async () => {
    const c = conversation(prompt, 'e');
    const input = { id: 'p1', text: 'Please move 4 from 2 to 3' };
    // More than the ten resumes OpenCode allows a turn by default.
    for (let n = 0; n < 12; n++) await c.crash('inv1', input);
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 4 from account 2 to account 3' });
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('finishes an interrupted prompt before starting the next one', async () => {
    const c = conversation(prompt, 'c');
    await c.crash('inv1', { id: 'p1', text: 'Please move 1 from 0 to 1' });
    // Its caller gave up; the next prompt comes.
    const second = await c.attempt('inv2', { id: 'p2', text: 'Please move 2 from 0 to 1' });
    expect(second).toEqual({ answer: 'Done: moved 2 from account 0 to account 1' });
    expect(await c.attempt('inv3', { id: 'p1', text: 'Please move 1 from 0 to 1' })).toEqual({
      answer: 'Done: moved 1 from account 0 to account 1',
    });
    expect(c.ledger.transfers.size).toBe(2);
  });
});

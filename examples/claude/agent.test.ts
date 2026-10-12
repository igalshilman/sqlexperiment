// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): the runner gets no chance to save anything on
// the way out of a crashed attempt. `npm run test:claude` (or make
// test-claude).
import { describe, expect, it } from 'vitest';
import { conversation } from '../testing.ts';
import { prompt } from './agent.ts';
import { teller } from './teller.ts';

describe('the Claude agent', () => {
  it('answers a prompt once, by its id', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', { id: 'p1', text: 'Please move 5 from 1 to 2' });
    expect(first).toEqual({ answer: 'Done: moved 5 from account 1 to account 2' });
    // Asked again, in a new invocation: the same answer, no new transfer.
    expect(await c.attempt('inv2', { id: 'p1', text: 'Please move 5 from 1 to 2' })).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect([...c.ledger.transfers.keys()]).toEqual(['ClaudeAgent/a/toolu_1']);
    // The next prompt goes on in the same thread.
    const audit = await c.attempt('inv3', { id: 'p2', text: 'audit the books' });
    expect(audit.answer).toMatch(/^Done: .*"transfers":1/);
  });

  it('resumes a crashed attempt from its last step, with the same tool call', async () => {
    const c = conversation(prompt, 'b');
    const calls = { model: 0 };
    const reply = teller.reply;
    teller.reply = (messages) => {
      calls.model++;
      return reply(messages);
    };
    try {
      const input = { id: 'p1', text: 'Please move 7 from 3 to 4' };
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt runs the tool again, not the model.
      expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 7 from account 3 to account 4' });
      expect(calls.model).toBe(2); // the answer after the tool
    } finally {
      teller.reply = reply;
    }
    // The same transfer id and idempotency key both times: the money moved once.
    expect(c.ledger.calls).toEqual([
      { name: 'transfer', id: 'ClaudeAgent/b/toolu_1', key: 'inv1:toolu_1' },
      { name: 'transfer', id: 'ClaudeAgent/b/toolu_1', key: 'inv1:toolu_1' },
    ]);
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

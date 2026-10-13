// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): the harness gets no chance to store anything on
// the way out of a crashed attempt, and the next attempt fences it, as Restate
// does. `npm run test:deepseek` (or make test-deepseek).
import { describe, expect, it } from 'vitest';
import type { RequestMessage } from '@deepseek-ai/dsh-llm';
import { conversation } from '../testing.ts';
import { prompt } from './agent.ts';
import { teller } from './teller.ts';

/** Count the model's replies while `body` runs. */
async function counting(body: (calls: { model: number }) => Promise<void>) {
  const calls = { model: 0 };
  const reply = teller.reply;
  teller.reply = (messages: RequestMessage[]) => {
    calls.model++;
    return reply(messages);
  };
  try {
    await body(calls);
  } finally {
    teller.reply = reply;
  }
}

describe('the DeepSeek Harness agent', () => {
  it('answers a prompt once, by its id', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', { id: 'p1', text: 'Please move 5 from 1 to 2' });
    expect(first).toEqual({ answer: 'Done: moved 5 from account 1 to account 2' });
    // Asked again, in a new invocation: the same answer, no new transfer.
    expect(await c.attempt('inv2', { id: 'p1', text: 'Please move 5 from 1 to 2' })).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect(c.ledger.calls[0]?.id).toMatch(/^DeepSeekAgent\/a\/call_[0-9a-f-]+$/);
    // The next prompt goes on in the same session.
    const audit = await c.attempt('inv3', { id: 'p2', text: 'audit the books' });
    expect(audit.answer).toMatch(/^Done: .*"transfers":1/);
  });

  it('retries a transfer whose outcome a crash left unknown, by its call id', async () => {
    const c = conversation(prompt, 'b');
    await counting(async (calls) => {
      const input = { id: 'p1', text: 'Please move 7 from 3 to 4' };
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt closes the interrupted turn and continues: the
      // model sees the unknown outcome and retries the transfer, then answers.
      expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 7 from account 3 to account 4' });
      expect(calls.model).toBe(3);
    });
    // The same transfer id and idempotency key both times: the money moved once.
    const [first, retried] = c.ledger.calls;
    expect(c.ledger.calls).toHaveLength(2);
    expect(retried).toEqual(first);
    expect(first?.key).toBe(`inv1:${first?.id.split('/').at(-1)}`);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('answers with a result stored before a crash, without running the tool again', async () => {
    const c = conversation(prompt, 'e');
    const input = { id: 'p1', text: 'Please move 2 from 6 to 7' };
    // The attempt crashes in the model's reply to the transfer's result.
    const reply = teller.reply;
    const crashed = new Promise<void>((resolve) => {
      teller.reply = (messages) => {
        if (messages.at(-1)?.role !== 'tool') return reply(messages);
        resolve();
        return new Promise(() => {});
      };
    });
    const attempt = c.attempt('inv1', input);
    await Promise.race([crashed, attempt.then(() => Promise.reject(new Error('did not crash')))]);
    teller.reply = reply;
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 2 from account 6 to account 7' });
    expect(c.ledger.calls).toHaveLength(1);
  });

  it("fails the attempt on a tool's error, rather than hand it to the model", async () => {
    const c = conversation(prompt, 'd');
    c.ledger.failNext = 1;
    const input = { id: 'p1', text: 'Please move 3 from 5 to 6' };
    await expect(c.attempt('inv1', input)).rejects.toThrow('crash after the transfer');
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 3 from account 5 to account 6' });
    expect(c.ledger.calls).toHaveLength(2);
    expect(c.ledger.calls[1]).toEqual(c.ledger.calls[0]);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('fails the attempt on a failed model request, and the retry asks again', async () => {
    const c = conversation(prompt, 'f');
    const input = { id: 'p1', text: 'Please move 4 from 6 to 7' };
    const reply = teller.reply;
    teller.reply = () => {
      teller.reply = reply;
      throw new Error('529 overloaded');
    };
    try {
      await expect(c.attempt('inv1', input)).rejects.toThrow("the agent's turn failed");
    } finally {
      teller.reply = reply;
    }
    // The failed turn is in the stored log: the retry continues past it, and
    // so does the next prompt.
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 4 from account 6 to account 7' });
    expect(c.ledger.transfers.size).toBe(1);
    const audit = await c.attempt('inv2', { id: 'p2', text: 'audit the books' });
    expect(audit.answer).toMatch(/^Done: .*"transfers":1/);
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

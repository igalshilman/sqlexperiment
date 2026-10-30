// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): the harness gets no chance to store anything on
// the way out of a crashed attempt, and the next attempt fences it, as Restate
// does. Its model is the teller (../fakes/deepseek.ts). `make test-ts T=deepseek`.
import { describe, expect, it, vi } from 'vitest';
import type { RequestMessage } from '@deepseek-ai/dsh-llm';
import * as restate from '@restatedev/restate-sdk';
import {
  afterCancel,
  cancelledTransfer,
  cancelledWhileWorking,
  STEERED,
  steeredWhileWorking,
  withoutIds,
  conversation,
  crashedAnywhere,
  SAID,
} from '../testing.ts';
import { teller } from '../fakes/deepseek.ts';
import * as deepseek from 'restate-agents/deepseek';
import { prompt } from './deepseek.ts';

vi.mock('@deepseek-ai/dsh-llm-deepseek-api-key', () => import('../fakes/deepseek.ts'));

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
  it('ends a turn cancelled at any moment as one cancelled while a tool waits', async () => {
    const now = await cancelledWhileWorking((tools) => deepseek.chat({ tools }), deepseek.messages());
    const waiting = await afterCancel(deepseek.chat({ tools: [cancelledTransfer] }), deepseek.messages());
    expect(now.failed).toBeInstanceOf(restate.CancelledError);
    expect(now.answer).toBe(waiting.answer);
    expect(withoutIds(now.parts)).toEqual(withoutIds(waiting.parts));
  });

  it('ends a cancelled turn, and answers the next message afresh', async () => {
    const { failed, answer, parts } = await afterCancel(
      deepseek.chat({ tools: [cancelledTransfer] }),
      deepseek.messages(),
    );
    expect(failed).toBeInstanceOf(restate.CancelledError);
    expect(answer).toBe('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
    // The harness ends the tool call with the cancellation, as its error.
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        {
          type: 'tool',
          id: expect.any(String),
          name: 'transfer',
          input: { from: 1, to: 2, amount: 5 },
          output: 'Error: Cancelled',
          error: true,
        },
      ],
      [{ type: 'text', text: 'Hello' }],
      [{ type: 'text', text: 'Ask me to "move 5 from 1 to 2", or to "audit" the books.' }],
    ]);
  });

  it('takes a message steered to the turn at its next tool call', async () => {
    const { answer, parts, left, published } = await steeredWhileWorking(
      (tools) => deepseek.chat({ tools }),
      deepseek.messages(),
    );
    expect(answer).toBe('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        {
          type: 'tool',
          name: 'transfer',
          input: { from: 1, to: 2, amount: 5 },
          output: 'moved 5 from account 1 to account 2',
        },
      ],
      [{ type: 'text', text: STEERED }],
      [{ type: 'text', text: answer }],
    ]);
    expect(left).toEqual([]);
    // Published at its start, as its tool call starts and ends, and at its end.
    expect(published).toBe(4);
  });

  it('answers a message once after a crash at any of its state requests', async () => {
    expect(await crashedAnywhere(prompt, deepseek.messages())).toEqual([]);
  }, 600_000);

  it('answers a message once after a crash at any of its state requests, steered once', async () => {
    expect(await crashedAnywhere(prompt, deepseek.messages(), { steered: true })).toEqual([]);
  }, 600_000);

  it('tells the model a tool it does not have is not there', async () => {
    // The teller audits with a tool `audit`, which this agent lacks.
    const c = conversation(deepseek.chat(), 'unknown');
    expect(await c.attempt('inv1', 'audit the books')).toMatch(/^Done: .*audit/s);
  });

  it('gives the conversation: what was said, in order, with the tool calls', async () => {
    const c = conversation(prompt, 'm');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    // Its teller calls tools without a word, unlike the others' "On it.".
    const said = [SAID[0], { ...SAID[1]!, parts: SAID[1]!.parts.slice(1) }, ...SAID.slice(2)];
    expect(await c.read(deepseek.messages())).toEqual({ messages: said, more: false, cursor: expect.any(Number) });
  });

  it('answers a message once per invocation', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', 'Please move 5 from 1 to 2');
    expect(first).toBe('Done: moved 5 from account 1 to account 2');
    // Its attempt retried: the same answer, no new transfer.
    expect(await c.attempt('inv1', 'Please move 5 from 1 to 2')).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect(c.ledger.calls[0]?.id).toMatch(/^DeepSeekAgent\/a\/call_[0-9a-f-]+$/);
    // The next prompt goes on in the same session.
    const audit = await c.attempt('inv3', 'audit the books');
    expect(audit).toMatch(/^Done: .*"transfers":1/);
  });

  it('retries a transfer whose outcome a crash left unknown, by its call id', async () => {
    const c = conversation(prompt, 'b');
    await counting(async (calls) => {
      const input = 'Please move 7 from 3 to 4';
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt closes the interrupted turn and continues: the
      // model sees the unknown outcome and retries the transfer, then answers.
      expect(await c.attempt('inv1', input)).toBe('Done: moved 7 from account 3 to account 4');
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
    const input = 'Please move 2 from 6 to 7';
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
    expect(await c.attempt('inv1', input)).toBe('Done: moved 2 from account 6 to account 7');
    expect(c.ledger.calls).toHaveLength(1);
  });

  it("fails the attempt on a tool's error, rather than hand it to the model", async () => {
    const c = conversation(prompt, 'd');
    c.ledger.failNext = 1;
    const input = 'Please move 3 from 5 to 6';
    await expect(c.attempt('inv1', input)).rejects.toThrow('crash after the transfer');
    expect(await c.attempt('inv1', input)).toBe('Done: moved 3 from account 5 to account 6');
    expect(c.ledger.calls).toHaveLength(2);
    expect(c.ledger.calls[1]).toEqual(c.ledger.calls[0]);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('fails the attempt on a failed model request, and the retry asks again', async () => {
    const c = conversation(prompt, 'f');
    const input = 'Please move 4 from 6 to 7';
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
    expect(await c.attempt('inv1', input)).toBe('Done: moved 4 from account 6 to account 7');
    expect(c.ledger.transfers.size).toBe(1);
    const audit = await c.attempt('inv2', 'audit the books');
    expect(audit).toMatch(/^Done: .*"transfers":1/);
  });

  it('finishes an interrupted prompt before starting the next one', async () => {
    const c = conversation(prompt, 'c');
    await c.crash('inv1', 'Please move 1 from 0 to 1');
    // Its caller gave up; the next prompt comes.
    const second = await c.attempt('inv2', 'Please move 2 from 0 to 1');
    expect(second).toBe('Done: moved 2 from account 0 to account 1');
    expect(await c.attempt('inv1', 'Please move 1 from 0 to 1')).toBe('Done: moved 1 from account 0 to account 1');
    expect(c.ledger.transfers.size).toBe(2);
  });
});

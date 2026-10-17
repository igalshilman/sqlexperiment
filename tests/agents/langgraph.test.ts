// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): LangGraph gets no chance to save anything on the
// way out of a crashed attempt. `make test-ts T=langgraph`. Its model is the teller (../fakes/langgraph.ts).
import { describe, expect, it, vi } from 'vitest';
import * as restate from '@restatedev/restate-sdk';
import { CANCELLED } from 'restate-agents';
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
import { Teller } from '../fakes/langgraph.ts';
import * as langgraph from 'restate-agents/langgraph';
import { prompt } from './langgraph.ts';

vi.mock('@langchain/anthropic', () => import('../fakes/langgraph.ts'));

describe('the LangGraph agent', () => {
  it('ends a turn cancelled at any moment as one cancelled while a tool waits', async () => {
    const now = await cancelledWhileWorking((tools) => langgraph.chat({ tools }), langgraph.messages());
    const waiting = await afterCancel(langgraph.chat({ tools: [cancelledTransfer] }), langgraph.messages());
    expect(now.failed).toBeInstanceOf(restate.CancelledError);
    expect(now.answer).toBe(waiting.answer);
    expect(withoutIds(now.parts)).toEqual(withoutIds(waiting.parts));
  });

  it('answers a message steered to the turn as a message of its own, once the turn ends', async () => {
    const { answer, left } = await steeredWhileWorking((tools) => langgraph.chat({ tools }), langgraph.messages());
    expect(answer).toBe('Done: moved 5 from account 1 to account 2');
    expect(left).toEqual([STEERED]);
  });

  it('ends a cancelled turn, and answers the next message afresh', async () => {
    const { failed, answer, parts } = await afterCancel(
      langgraph.chat({ tools: [cancelledTransfer] }),
      langgraph.messages(),
    );
    expect(failed).toBeInstanceOf(restate.CancelledError);
    expect(answer).toBe('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        { type: 'text', text: 'On it.' },
        {
          type: 'tool',
          id: 'call_1',
          name: 'transfer',
          input: { from: 1, to: 2, amount: 5 },
          output: CANCELLED,
          error: true,
        },
      ],
      [{ type: 'text', text: CANCELLED }],
      [{ type: 'text', text: 'Hello' }],
      [{ type: 'text', text: 'Ask me to "move 5 from 1 to 2", or to "audit" the books.' }],
    ]);
  });

  it('answers a message once after a crash at any of its state requests', async () => {
    expect(await crashedAnywhere(prompt, langgraph.messages())).toEqual([]);
  }, 600_000);

  it('answers a message once after a crash at any of its state requests, steered once', async () => {
    expect(await crashedAnywhere(prompt, langgraph.messages(), { steered: true })).toEqual([]);
  }, 600_000);

  it('tells the model a tool it does not have is not there', async () => {
    // The teller audits with a tool `audit`, which this agent lacks.
    const c = conversation(langgraph.chat(), 'unknown');
    expect(await c.attempt('inv1', 'audit the books')).toMatch(/^Done: .*audit/s);
  });

  it('gives the conversation: what was said, in order, with the tool calls', async () => {
    const c = conversation(prompt, 'm');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    expect(await c.read(langgraph.messages())).toEqual({ messages: SAID, more: false });
  });

  it('gives the model its system prompt', async () => {
    const c = conversation(langgraph.chat({ system: 'You are Ada.' }), 'ada');
    expect(await c.attempt('inv1', 'Who are you?')).toBe('You are Ada.');
  });

  it('answers a message once per invocation', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', 'Please move 5 from 1 to 2');
    expect(first).toBe('Done: moved 5 from account 1 to account 2');
    // Its attempt retried: the same answer, no new transfer.
    expect(await c.attempt('inv1', 'Please move 5 from 1 to 2')).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect([...c.ledger.transfers.keys()]).toEqual(['GraphAgent/a/call_1']);
    // The next prompt goes on in the same thread.
    const audit = await c.attempt('inv3', 'audit the books');
    expect(audit).toMatch(/^Done: .*"transfers":1/);
  });

  it('resumes a crashed attempt from its last step, with the same tool call', async () => {
    const c = conversation(prompt, 'b');
    const reply = vi.spyOn(Teller.prototype, 'reply');
    try {
      const input = 'Please move 7 from 3 to 4';
      await c.crash('inv1', input);
      expect(reply).toHaveBeenCalledTimes(1);
      // The retried attempt runs the tool again, not the model.
      expect(await c.attempt('inv1', input)).toBe('Done: moved 7 from account 3 to account 4');
      expect(reply).toHaveBeenCalledTimes(2); // the answer after the tool
    } finally {
      reply.mockRestore();
    }
    // The same transfer id and idempotency key both times: the money moved once.
    expect(c.ledger.calls).toEqual([
      { name: 'transfer', id: 'GraphAgent/b/call_1', key: 'inv1:call_1' },
      { name: 'transfer', id: 'GraphAgent/b/call_1', key: 'inv1:call_1' },
    ]);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it("fails the attempt on a tool's error, rather than hand it to the model", async () => {
    const c = conversation(prompt, 'd');
    c.ledger.failNext = 1;
    const input = 'Please move 3 from 5 to 6';
    await expect(c.attempt('inv1', input)).rejects.toThrow('crash after the transfer');
    expect(await c.attempt('inv1', input)).toBe('Done: moved 3 from account 5 to account 6');
    expect(c.ledger.transfers.size).toBe(1);
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

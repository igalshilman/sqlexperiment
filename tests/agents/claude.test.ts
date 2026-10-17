// The agent's handler without a Restate server, through crashes of the
// handler (see ../testing.ts): the runner gets no chance to save anything on
// the way out of a crashed attempt. `make test-ts T=claude`. Its client is the teller's (../fakes/claude.ts).
import * as restate from '@restatedev/restate-sdk';
import { describe, expect, it, vi } from 'vitest';
import { CANCELLED, tool } from 'restate-agents';
import { z } from 'zod';
import {
  afterCancel,
  cancelledTransfer,
  cancelledWhileWorking,
  STEERED,
  steeredWhileWorking,
  conversation,
  crashedAnywhere,
  SAID,
  sqlAttempt,
  withoutIds,
} from '../testing.ts';
import { HttpError, teller } from '../fakes/claude.ts';
import * as claude from 'restate-agents/claude';
import { bank } from './bank.ts';
import { prompt } from './claude.ts';

vi.mock('@anthropic-ai/sdk', () => import('../fakes/claude.ts'));

/** A handler that prompts twice, with the bank's tools. */
const twice = async (ctx: restate.SqlObjectContext) =>
  JSON.stringify([
    await claude.prompt(ctx, 'Please move 5 from 1 to 2', { tools: bank('ClaudeAgent') }),
    await claude.prompt(ctx, 'Hello', { tools: bank('ClaudeAgent') }),
  ]);

describe('the Claude agent', () => {
  it('ends a turn cancelled at any moment as one cancelled while a tool waits', async () => {
    const now = await cancelledWhileWorking((tools) => claude.chat({ tools }), claude.messages());
    const waiting = await afterCancel(claude.chat({ tools: [cancelledTransfer] }), claude.messages());
    expect(now.failed).toBeInstanceOf(restate.CancelledError);
    expect(now.answer).toBe(waiting.answer);
    expect(withoutIds(now.parts)).toEqual(withoutIds(waiting.parts));
  });

  it('answers a message steered to the turn as a message of its own, once the turn ends', async () => {
    const { answer, left, published } = await steeredWhileWorking((tools) => claude.chat({ tools }), claude.messages());
    expect(answer).toBe('Done: moved 5 from account 1 to account 2');
    expect(left).toEqual([STEERED]);
    // Published at its start, as its tool call starts and ends, and at its end.
    expect(published).toBe(4);
  });

  it('ends a cancelled turn, and answers the next message afresh', async () => {
    const { failed, answer, parts } = await afterCancel(claude.chat({ tools: [cancelledTransfer] }), claude.messages());
    expect(failed).toBeInstanceOf(restate.CancelledError);
    expect(answer).toBe('Ask me to "move 5 from 1 to 2", or to "audit" the books.');
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        { type: 'text', text: 'On it.' },
        {
          type: 'tool',
          id: 'toolu_1',
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

  it('gives the conversation while it answers: the tool calls made so far', async () => {
    const map = new Map<string, Uint8Array>();
    const started = Promise.withResolvers<void>();
    const gate = Promise.withResolvers<void>();
    const transfer = tool({
      name: 'transfer',
      description: 'Move money.',
      input: z.object({ from: z.number(), to: z.number(), amount: z.number() }),
      run: async () => {
        started.resolve();
        await gate.promise;
        return 'moved 5 from account 1 to account 2';
      },
    });
    const owner = sqlAttempt({ map, key: 'mid', id: 'inv1' });
    const ctx = { key: 'mid', ...owner };
    const answer = owner.end(() =>
      claude.chat({ tools: [transfer] })(ctx as unknown as restate.SqlObjectContext, 'Please move 5 from 1 to 2'),
    );
    await started.promise;
    const reader = sqlAttempt({ map, key: 'mid', id: 'read', shared: true });
    const read = { key: 'mid', ...reader };
    const page = await reader.end(() => claude.messages()(read as unknown as restate.SqlObjectContext, {}));
    expect(page.messages.map((m) => m.parts)).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        { type: 'text', text: 'On it.' },
        { type: 'tool', id: 'toolu_1', name: 'transfer', input: { from: 1, to: 2, amount: 5 } },
      ],
    ]);
    gate.resolve();
    expect(await answer).toBe('Done: moved 5 from account 1 to account 2');
  });

  it('answers a message once after a crash at any of its state requests', async () => {
    expect(await crashedAnywhere(prompt, claude.messages())).toEqual([]);
  }, 600_000);

  it('answers a message once after a crash at any of its state requests, steered once', async () => {
    expect(await crashedAnywhere(prompt, claude.messages(), { steered: true })).toEqual([]);
  }, 600_000);

  it('tells the model a tool it does not have is not there', async () => {
    // The teller audits with a tool `audit`, which this agent lacks.
    const c = conversation(claude.chat(), 'unknown');
    expect(await c.attempt('inv1', 'audit the books')).toMatch(/^Done: .*audit/s);
  });

  it('gives the conversation: what was said, in order, with the tool calls', async () => {
    const c = conversation(prompt, 'm');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    expect(await c.read(claude.messages())).toEqual({ messages: SAID, more: false });
  });

  it('gives the conversation a page at a time', async () => {
    const c = conversation(prompt, 'p');
    await c.attempt('inv1', 'Please move 5 from 1 to 2');
    await c.attempt('inv2', 'audit the books');
    // The latest, then older ones before the oldest so far.
    expect(await c.read(claude.messages(), { limit: 2 })).toEqual({ messages: SAID.slice(4), more: true });
    expect(await c.read(claude.messages(), { limit: 3, before: 4 })).toEqual({
      messages: SAID.slice(1, 4),
      more: true,
    });
    expect(await c.read(claude.messages(), { before: 1 })).toEqual({ messages: SAID.slice(0, 1), more: false });
    // Newer ones after the newest so far.
    expect(await c.read(claude.messages(), { after: 2, limit: 2 })).toEqual({ messages: SAID.slice(3, 5), more: true });
    expect(await c.read(claude.messages(), { after: 5 })).toEqual({ messages: [], more: false });
  });

  it('answers a message once per invocation', async () => {
    const c = conversation(prompt, 'a');
    const first = await c.attempt('inv1', 'Please move 5 from 1 to 2');
    expect(first).toBe('Done: moved 5 from account 1 to account 2');
    // Its attempt retried: the same answer, no new transfer.
    expect(await c.attempt('inv1', 'Please move 5 from 1 to 2')).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect([...c.ledger.transfers.keys()]).toEqual(['ClaudeAgent/a/toolu_1']);
    // The next prompt goes on in the same thread.
    const audit = await c.attempt('inv3', 'audit the books');
    expect(audit).toMatch(/^Done: .*"transfers":1/);
  });

  it('answers each prompt of an invocation, and each once', async () => {
    const c = conversation(twice, 't');
    const both = JSON.stringify([
      'Done: moved 5 from account 1 to account 2',
      'Ask me to "move 5 from 1 to 2", or to "audit" the books.',
    ]);
    expect(await c.attempt('inv1', '')).toBe(both);
    expect(await c.attempt('inv1', '')).toBe(both);
    expect(c.ledger.calls).toHaveLength(1);
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
      const input = 'Please move 7 from 3 to 4';
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt runs the tool again, not the model.
      expect(await c.attempt('inv1', input)).toBe('Done: moved 7 from account 3 to account 4');
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
    const input = 'Please move 3 from 5 to 6';
    const failed = await c.attempt('inv1', input).catch((error: unknown) => error);
    expect(failed).toBeInstanceOf(Error);
    expect((failed as Error).message).toMatch('crash after the transfer');
    // Not terminal: Restate retries the attempt.
    expect(failed).not.toBeInstanceOf(restate.TerminalError);
    expect(await c.attempt('inv1', input)).toBe('Done: moved 3 from account 5 to account 6');
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('fails the attempt on a failed model request, and the retry goes on', async () => {
    const c = conversation(prompt, 'e');
    const reply = teller.reply;
    teller.reply = () => {
      teller.reply = reply;
      throw new HttpError(529, 'overloaded_error', 'Overloaded');
    };
    const input = 'Please move 4 from 6 to 7';
    try {
      const failed = await c.attempt('inv1', input).catch((error: unknown) => error);
      // The model's own error, not a terminal one: the attempt is retried.
      expect(failed).toBeInstanceOf(Error);
      expect((failed as Error).message).toMatch(/^529 .*Overloaded/);
      expect(failed).not.toBeInstanceOf(restate.TerminalError);
    } finally {
      teller.reply = reply;
    }
    expect(await c.attempt('inv1', input)).toBe('Done: moved 4 from account 6 to account 7');
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

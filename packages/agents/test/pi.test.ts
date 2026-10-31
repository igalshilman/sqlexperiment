// pi's chat without a Restate server or an API key: Anthropic's provider is a
// fake model (../fakes/pi.ts).
import * as restate from 'restate-sessions';
import { subagent, tool } from 'restate-agents';
import { z } from 'zod';
import * as pi from 'restate-agents/pi';
import { describe, expect, it, vi } from 'vitest';
import { callTools, fakeProvider, say, textOf } from './fakes/pi.ts';
import {
  afterCancel,
  cancelledTransfer,
  cancelledWhileWorking,
  invalidArguments,
  STEERED,
  steeredWhileWorking,
  withoutIds,
  type Made,
  type Entry,
  handlerOf,
  sqlAttempt,
} from './support.ts';

// It repeats what it was told so far; asked to delegate, it hands out two
// tasks, and then reports their results.
vi.mock('@earendil-works/pi-ai/providers/all', () => ({
  builtinProviders: () => [
    fakeProvider('openai', 'gpt-6-sol', ({ messages }) => {
      const last = messages.at(-1)!;
      if (last.role === 'toolResult') {
        return say(
          messages
            .filter((m) => m.role === 'toolResult')
            .map((m) => textOf(m.content))
            .join('\n'),
        );
      }
      // Asked who it is, it repeats its system prompt.
      if (textOf(last.content) === 'Who are you?') {
        const system = messages.find((m) => m.role === 'system') as { sections?: Record<string, string> } | undefined;
        return say(Object.values(system?.sections ?? {}).join('\n'));
      }
      if (textOf(last.content) === 'Break it') return callTools([['boom', {}]]);
      if (textOf(last.content) === 'Look it up') return callTools([['lookup', {}]]);
      if (textOf(last.content) === 'Please move 5 from 1 to 2') {
        return callTools([['transfer', { from: 1, to: 2, amount: 5 }]], 'On it.');
      }
      if (textOf(last.content).startsWith('delegate')) {
        return callTools([
          ['coder', { name: 'fizzbuzz', task: 'write fizzbuzz.py' }],
          ['coder', { name: 'primes', task: 'write primes.py' }],
        ]);
      }
      return say(
        messages
          .filter((m) => m.role === 'user')
          .map((m) => textOf(m.content))
          .join(' / '),
      );
    }),
  ],
}));

interface Call {
  service: string;
  key?: string;
  method: string;
  parameter: unknown;
  idempotencyKey?: string;
}

/** An object with key `key`: its database lives across invocations, and its calls are recorded. */
function object(key: string) {
  const map = new Map<string, Uint8Array>();
  const calls: Call[] = [];
  const route = async (call: Made) => {
    calls.push(call);
    return `${call.key}: done`;
  };
  return {
    calls,
    /** Invocation `id` of `handler`. */
    invoke<I, O>(handler: Entry<(ctx: restate.SessionContext, input: I) => Promise<O>>, id: string, input: I) {
      const sql = sqlAttempt({ map, key, id, route });
      return sql.end(() => handlerOf(handler)(sql.ctx, input));
    },
  };
}

/** Message `id` of `messages`: `text` of `role`. */
const said = (id: number, role: string, text: string) => ({ id, role, parts: [{ type: 'text', text }] });

describe('pi.messages', () => {
  it('answers arguments that do not fit a tool with what is wrong, for the model to fix', async () => {
    const { answer, ran } = await invalidArguments((tools) => pi.chat({ tools }));
    expect(answer).toMatch(/error/i);
    expect(answer).toMatch(/amount/);
    expect(ran).toBe(false);
  });

  it("marks a tool's error", async () => {
    const alice = object('alice');
    const lookup = tool({
      name: 'lookup',
      description: 'Finds nothing.',
      input: z.object({}),
      run: () => {
        throw new restate.TerminalError('no such page');
      },
    });
    await alice.invoke(pi.chat({ tools: [lookup] }), 'inv1', 'Look it up');
    const { messages } = await alice.invoke(pi.messages(), 'read', {});
    expect(messages[1]?.parts).toEqual([
      { type: 'tool', id: expect.any(String), name: 'lookup', input: {}, output: 'Error: no such page', error: true },
    ]);
  });

  it("reads without MODEL's provider", async () => {
    const alice = object('alice');
    await alice.invoke(pi.chat(), 'inv1', 'Hi, I am Alice');
    vi.stubEnv('MODEL', 'nowhere/nothing');
    try {
      expect((await alice.invoke(pi.messages(), 'read', {})).messages).toHaveLength(2);
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it('gives the conversation: what was said, in order, a page at a time', async () => {
    const alice = object('alice');
    await alice.invoke(pi.chat(), 'inv1', 'Hi, I am Alice');
    await alice.invoke(pi.chat(), 'inv2', 'What is my name?');
    expect(await alice.invoke(pi.messages(), 'read', {})).toEqual({
      messages: [
        said(0, 'user', 'Hi, I am Alice'),
        said(1, 'assistant', 'Hi, I am Alice'),
        said(2, 'user', 'What is my name?'),
        said(3, 'assistant', 'Hi, I am Alice / What is my name?'),
      ],
      more: false,
      cursor: expect.any(Number),
    });
    expect(await alice.invoke(pi.messages(), 'read2', { limit: 1, before: 3 })).toEqual({
      messages: [said(2, 'user', 'What is my name?')],
      more: true,
      cursor: expect.any(Number),
    });
  });

  it('keeps what was said through a reset, and follows on after it with new ids', async () => {
    const alice = object('alice');
    await alice.invoke(pi.chat(), 'inv1', 'Hi, I am Alice');
    const { cursor } = await alice.invoke(pi.messages(), 'read', {});
    await alice.invoke(reset, 'inv2', undefined);
    await alice.invoke(pi.chat(), 'inv3', 'Who am I?');
    // The model starts over (it does not know Alice); clients still read what was said.
    const since = await alice.invoke(pi.messages(), 'read2', { cursor });
    expect(since.messages).toEqual([said(2, 'user', 'Who am I?'), said(3, 'assistant', 'Who am I?')]);
    const all = await alice.invoke(pi.messages(), 'read3', {});
    expect(all.messages.map((m: { id: number }) => m.id)).toEqual([0, 1, 2, 3]);
  });
});

/** A handler that starts the conversation over. */
const reset = (ctx: restate.SessionContext) =>
  pi.withPi(ctx, {}, async ({ conversation, context }) => (await conversation()).reset(undefined, context));

/** A handler that prompts twice. */
const twice = async (ctx: restate.SessionContext) => [
  await pi.prompt(ctx, 'One'),
  await pi.withPi(ctx, {}, (session) => session.prompt('Two')),
];

describe('pi.chat', () => {
  it('ends a turn cancelled at any moment as one cancelled while a tool waits', async () => {
    const now = await cancelledWhileWorking((tools) => pi.chat({ tools }), pi.messages());
    const waiting = await afterCancel(pi.chat({ tools: [cancelledTransfer] }), pi.messages());
    expect(now.failed).toBeInstanceOf(restate.CancelledError);
    expect(now.answer).toBe(waiting.answer);
    expect(withoutIds(now.parts)).toEqual(withoutIds(waiting.parts));
  });

  it('ends a cancelled turn, and answers the next message afresh', async () => {
    const { failed, answer, parts } = await afterCancel(pi.chat({ tools: [cancelledTransfer] }), pi.messages());
    expect(failed).toBeInstanceOf(restate.CancelledError);
    expect(answer).toBe('Please move 5 from 1 to 2 / Hello');
    // pi-durable's abort ends the tool call, as its own error.
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        { type: 'text', text: 'On it.' },
        {
          type: 'tool',
          id: expect.any(String),
          name: 'transfer',
          input: { from: 1, to: 2, amount: 5 },
          output: expect.stringContaining('Tool transfer was aborted'),
          error: true,
        },
      ],
      [{ type: 'text', text: 'Hello' }],
      [{ type: 'text', text: 'Please move 5 from 1 to 2 / Hello' }],
    ]);
  });

  it('takes a message steered to the turn at its next tool call', async () => {
    const { answer, parts, left, published } = await steeredWhileWorking((tools) => pi.chat({ tools }), pi.messages());
    expect(answer).toBe(`Please move 5 from 1 to 2 / ${STEERED}`);
    expect(parts).toEqual([
      [{ type: 'text', text: 'Please move 5 from 1 to 2' }],
      [
        { type: 'text', text: 'On it.' },
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

  it('in the background, publishes only the end of a turn, which answers a steered message after it', async () => {
    pi.agent({ name: 'unwatched', background: true });
    const { answer, left, published } = await steeredWhileWorking((tools) => pi.chat({ tools }), pi.messages(), {
      service: 'unwatched',
    });
    expect(answer).not.toContain(STEERED);
    expect(left).toEqual([STEERED]);
    expect(published).toBe(1);
  });

  it('takes options for each invocation', async () => {
    const ada = object('ada');
    const chat = pi.chat(async (ctx) => ({ system: `You are ${ctx.key}.` }));
    expect(await ada.invoke(chat, 'inv1', 'Who are you?')).toContain('You are ada.');
  });

  it('gives the model its system prompt', async () => {
    const ada = object('ada');
    expect(await ada.invoke(pi.chat({ system: 'You are Ada.' }), 'inv1', 'Who are you?')).toContain('You are Ada.');
  });

  it("fails the attempt on a tool's error, and the retry runs the tool call again", async () => {
    const alice = object('alice');
    const calls: string[] = [];
    const boom = tool({
      name: 'boom',
      description: 'Fails the first time.',
      input: z.object({}),
      run: async (_input, { callId }) => {
        calls.push(callId);
        if (calls.length === 1) throw new Error('the disk is on fire');
        return 'fixed';
      },
    });
    const chat = pi.chat({ tools: [boom] });
    await expect(alice.invoke(chat, 'inv1', 'Break it')).rejects.toThrow('the disk is on fire');
    expect(await alice.invoke(chat, 'inv1', 'Break it')).toBe('fixed');
    expect(calls).toHaveLength(2);
    expect(calls[1]).toBe(calls[0]);
  });

  it('answers each prompt of an invocation, and each once', async () => {
    const alice = object('alice');
    expect(await alice.invoke(twice, 'inv1', undefined)).toEqual(['One', 'One / Two']);
    expect(await alice.invoke(twice, 'inv1', undefined)).toEqual(['One', 'One / Two']);
  });

  it('remembers the conversation across messages, and answers a retried message once', async () => {
    const alice = object('alice');
    const chat = pi.chat();
    expect(await alice.invoke(chat, 'inv1', 'Hi, I am Alice')).toBe('Hi, I am Alice');
    expect(await alice.invoke(chat, 'inv2', 'What is my name?')).toBe('Hi, I am Alice / What is my name?');
    // The same invocation again: its prompt is answered already.
    expect(await alice.invoke(chat, 'inv2', 'What is my name?')).toBe('Hi, I am Alice / What is my name?');
  });
});

/** A sub-agent with a `code` handler. */
const coder = { name: 'coder' } as restate.VirtualObjectDefinition<
  'coder',
  { code: (ctx: restate.SessionContext, task: string) => Promise<string> }
>;

describe('subagent', () => {
  it('hands each task to the sub-agent of its name, once per tool call', async () => {
    const alice = object('alice');
    const chat = pi.chat({ tools: [subagent(coder, { handler: 'code' })] });
    expect(await alice.invoke(chat, 'inv1', 'delegate fizzbuzz and primes')).toBe(
      'alice.fizzbuzz: done\nalice.primes: done',
    );
    expect(alice.calls.map((c) => [c.service, c.key, c.method, c.parameter])).toEqual([
      ['coder', 'alice.fizzbuzz', 'code', 'write fizzbuzz.py'],
      ['coder', 'alice.primes', 'code', 'write primes.py'],
    ]);
    // One idempotency key per tool call, made from the invocation id.
    expect(new Set(alice.calls.map((c) => c.idempotencyKey)).size).toBe(2);
    expect(alice.calls.every((c) => c.idempotencyKey?.startsWith('inv1:'))).toBe(true);
  });
});

// pi's chat without a Restate server or an API key: Anthropic's provider is a
// fake model (../fakes/pi.ts).
import type * as restate from '@restatedev/restate-sdk';
import { subagent } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { describe, expect, it, vi } from 'vitest';
import { callTools, fakeProvider, say, textOf } from '../fakes/pi.ts';
import { sqlAttempt } from '../testing.ts';

// It repeats what it was told so far; asked to delegate, it hands out two
// tasks, and then reports their results.
vi.mock('@earendil-works/pi-ai/providers/anthropic', () => ({
  anthropicProvider: () =>
    fakeProvider('anthropic', 'claude-sonnet-5-5', ({ messages }) => {
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
}));

type Handler = (ctx: restate.SqlObjectContext, message: string) => Promise<string>;
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
  return {
    calls,
    /** Invocation `id` of `handler`. */
    invoke(handler: Handler, id: string, message: string) {
      const sql = sqlAttempt({ map, key, id });
      const ctx = {
        key,
        request: () => ({ id }),
        db: sql.db,
        transaction: sql.transaction,
        genericCall: async (call: Call) => {
          calls.push(call);
          return `${call.key}: done`;
        },
      };
      return sql.end(() => handler(ctx as unknown as restate.SqlObjectContext, message));
    },
  };
}

describe('pi.messages', () => {
  it('gives the conversation: what was said, in order', async () => {
    const alice = object('alice');
    await alice.invoke(pi.chat(), 'inv1', 'Hi, I am Alice');
    await alice.invoke(pi.chat(), 'inv2', 'What is my name?');
    expect(await alice.invoke(pi.messages() as unknown as Handler, 'read', '')).toEqual([
      { role: 'user', text: 'Hi, I am Alice' },
      { role: 'assistant', text: 'Hi, I am Alice' },
      { role: 'user', text: 'What is my name?' },
      { role: 'assistant', text: 'Hi, I am Alice / What is my name?' },
    ]);
  });
});

describe('pi.chat', () => {
  it('gives the model its system prompt', async () => {
    const ada = object('ada');
    expect(await ada.invoke(pi.chat({ system: 'You are Ada.' }), 'inv1', 'Who are you?')).toContain('You are Ada.');
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
const coder = { name: 'coder' } as restate.VirtualObjectDefinition<'coder', { code: Handler }>;

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

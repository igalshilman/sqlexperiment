// The chat, without a Restate server or an API key: Anthropic's provider is a
// fake model that repeats what it was told so far.
import type * as restate from '@restatedev/restate-sdk';
import { describe, expect, it, vi } from 'vitest';
import { sqlAttempt } from '../testing.ts';
import { fakeProvider, say, textOf } from '../testing/pi.ts';
import { session } from './chat.ts';

vi.mock('@earendil-works/pi-ai/providers/anthropic', () => ({
  anthropicProvider: () =>
    fakeProvider('anthropic', 'claude-sonnet-5-5', ({ messages }) =>
      say(
        messages
          .filter((m) => m.role === 'user')
          .map((m) => textOf(m.content))
          .join(' / '),
      ),
    ),
}));

/** The object's handler, as written in chat.ts. */
const { chat: handler } = (
  session as unknown as { object: { chat: (ctx: restate.SqlObjectContext, m: string) => Promise<string> } }
).object;

describe('session.chat', () => {
  it('remembers the conversation across messages, and answers a retried message once', async () => {
    const map = new Map<string, Uint8Array>();
    const chat = (id: string, message: string) => {
      const sql = sqlAttempt({ map, key: 'alice', id });
      const ctx = { key: 'alice', request: () => ({ id }), db: sql.db, transaction: sql.transaction };
      return sql.end(() => handler(ctx as unknown as restate.SqlObjectContext, message));
    };
    expect(await chat('inv1', 'Hi, I am Alice')).toBe('Hi, I am Alice');
    expect(await chat('inv2', 'What is my name?')).toBe('Hi, I am Alice / What is my name?');
    // The same invocation again: its prompt is answered already.
    expect(await chat('inv2', 'What is my name?')).toBe('Hi, I am Alice / What is my name?');
  });
});

// The conversation as a person reads it (asText of restate-agents/client).
import { asText } from 'restate-agents/client';
import { describe, expect, it } from 'vitest';

describe('a message as text', () => {
  it('is its text after its role, and each tool call on a line of its own, with its output', () => {
    const said = asText({
      id: 3,
      role: 'assistant',
      parts: [
        { type: 'text', text: 'Moving it.' },
        { type: 'tool', id: 'c1', name: 'transfer', input: { amount: 5 }, output: 'moved 5' },
        { type: 'tool', id: 'c2', name: 'audit', input: {} },
      ],
    });
    expect(said).toBe('assistant: Moving it.\n  transfer({"amount":5}) -> moved 5\n  audit({}) -> …');
  });
});

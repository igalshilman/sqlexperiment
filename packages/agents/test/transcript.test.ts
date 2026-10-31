// Transcript: a message it gave out is never changed, so the log stores only
// the messages that are not the ones it stored last (log.ts).
import { describe, expect, it } from 'vitest';
import { Transcript } from '../src/common.ts';

describe('a transcript', () => {
  it('makes a new message in place of one that changes, and keeps the others', () => {
    const transcript = new Transcript();
    transcript.say('user', 'Move 5');
    transcript.say('assistant', 'On it.');
    transcript.call('call_1', 'transfer', { amount: 5 });
    const [asked, before] = transcript.messages();
    transcript.result('call_1', 'moved');
    const [still, after] = transcript.messages();
    expect(still).toBe(asked);
    expect(after).not.toBe(before);
    expect(before!.parts).toEqual([
      { type: 'text', text: 'On it.' },
      { type: 'tool', id: 'call_1', name: 'transfer', input: { amount: 5 } },
    ]);
    expect(after!.parts.at(-1)).toEqual({
      type: 'tool',
      id: 'call_1',
      name: 'transfer',
      input: { amount: 5 },
      output: 'moved',
    });
    expect(transcript.hasResult('call_1')).toBe(true);
  });

  it("marks a tool's error", () => {
    const transcript = new Transcript();
    transcript.call('call_1', 'transfer', {});
    transcript.call('call_2', 'audit', {});
    transcript.result('call_1', 'Error: no such account');
    transcript.result('call_2', 'refused', true);
    expect(transcript.messages()[0]!.parts).toMatchObject([{ error: true }, { error: true }]);
  });
});

// The gateway's parts without a Restate server: the prebuilt tools and the
// scripted model. Against a server: demo.sh.
import type { TranscriptContext } from '@earendil-works/pi-ai';
import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { caseworker, concierge, EXAMPLES, investigates, PROMPT_SCHEMA, sentence } from './agent.ts';
import { identity } from './gateway.ts';
import { PREBUILT } from './tools.ts';

describe('the gateway', () => {
  it('has prebuilt tools, with the JSON schema of their zod input', () => {
    expect(Object.keys(PREBUILT)).toEqual(['lookup_order', 'refund', 'open_ticket']);
    expect(PREBUILT.refund).toMatchObject({ approval: true, service: 'Payments', handler: 'refund' });
    expect(PREBUILT.lookup_order!.inputSchema).toEqual({
      type: 'object',
      properties: { order: { type: 'string', description: 'The order id, such as "101"' } },
      required: ['order'],
      additionalProperties: false,
    });
  });

  it('takes the tenant and the user from `<tenant>.<user>`', () => {
    expect(identity('acme.alice')).toEqual({ tenant: 'acme', user: 'alice' });
    expect(identity('acme')).toEqual({ tenant: 'acme', user: 'anonymous' });
  });
});

const user = (content: string) => ({ role: 'user', content, timestamp: 0 });
const toolCalls = (...messages: unknown[]) =>
  concierge({ messages } as TranscriptContext).content.flatMap((c) =>
    c.type === 'toolCall' ? [[c.name, c.arguments]] : [],
  );
const replyText = (model: typeof concierge, ...messages: unknown[]) =>
  model({ messages } as unknown as TranscriptContext).content.flatMap((c) =>
    c.type === 'text' ? [c.text as unknown] : c.type === 'toolCall' ? [[c.name, c.arguments]] : [],
  );

const result = (text: string) => ({ role: 'toolResult', content: [{ type: 'text', text }], isError: false });

describe('the prompt', () => {
  it('takes a string, and suggests the first example', () => {
    // An input the SDK announces: an empty body is not one.
    expect(PROMPT_SCHEMA['~standard'].validate(undefined)).toHaveProperty('issues');
    expect(z.toJSONSchema(PROMPT_SCHEMA)).toMatchObject({ type: 'string', default: EXAMPLES[0], examples: EXAMPLES });
  });
});

describe('the scripted model', () => {
  it('looks up a range of orders at once, and refunds', () => {
    expect(toolCalls(user('where are orders 101 to 103?'))).toEqual([
      ['lookup_order', { order: '101' }],
      ['lookup_order', { order: '102' }],
      ['lookup_order', { order: '103' }],
    ]);
    expect(toolCalls(user('refund 30 of order 103'))).toEqual([['refund', { order: '103', amount: 30 }]]);
    expect(toolCalls(user('Open a ticket for order 104: the box was empty.'))).toEqual([
      ['open_ticket', { order: '104', summary: 'the box was empty.' }],
    ]);
  });

  it('understands every example prompt', () => {
    for (const example of EXAMPLES) expect(toolCalls(user(example)).length).toBeGreaterThan(0);
  });

  it('says what a tool answered, in a sentence', () => {
    expect(sentence('{"id":"101","status":"delivered","total":99}')).toBe('Order 101 is delivered, $99.');
    expect(sentence('{"refund":"re_0"}')).toBe('Refund re_0 is made.');
    expect(sentence('globex has no tool refund')).toBe('globex has no tool refund');
  });

  it("reports the last round's results", () => {
    const reply = concierge({ messages: [user('…'), result('one'), result('two')] } as unknown as TranscriptContext);
    expect(reply.content).toEqual([{ type: 'text', text: 'one\ntwo' }]);
  });
});

describe('case agents', () => {
  it('are asked for only by a prompt that begins "Investigate"', () => {
    expect(investigates('Investigate orders 103 and 104: crushed.')).toBe(true);
    expect(investigates('Where are orders 101 to 103? Investigate them.')).toBe(false);
    expect(toolCalls(user('Investigate orders 103 and 104: the boxes arrived crushed.'))).toEqual([
      ['investigate', { order: '103', question: 'the boxes arrived crushed.' }],
      ['investigate', { order: '104', question: 'the boxes arrived crushed.' }],
    ]);
  });

  it('look the order up, open a ticket, and remember earlier reports', () => {
    const first = user('Order 103, from alice: crushed.');
    expect(replyText(caseworker, first)).toEqual([
      ['lookup_order', { order: '103' }],
      ['open_ticket', { order: '103', summary: 'alice: crushed.' }],
    ]);
    const results = [result('{"id":"103","status":"shipped","total":40}'), result('{"ticket":"T-1000"}')];
    expect(replyText(caseworker, first, ...results)).toEqual([
      'Case 103: Order 103 is shipped, $40. Ticket T-1000 is open. The first report on it.',
    ]);
    const second = user('Order 103, from bob: still crushed.');
    expect(replyText(caseworker, first, ...results, { role: 'assistant' }, second, ...results)).toEqual([
      'Case 103: Order 103 is shipped, $40. Ticket T-1000 is open. Reported before: alice: "crushed.".',
    ]);
  });
});

// The gateway's parts without a Restate server: the registry and the
// scripted model. Against a server: demo.sh.
import type { TranscriptContext } from '@earendil-works/pi-ai';
import { describe, expect, it } from 'vitest';
import { concierge } from './agent.ts';
import { identity, toolsOf } from './gateway.ts';

describe('the registry', () => {
  it('lists the handlers that name a tool, with their schema and approval', () => {
    const tools = toolsOf({
      services: [
        {
          name: 'Payments',
          handlers: [
            {
              name: 'refund',
              documentation: 'Refund an order.',
              metadata: { 'mcp.tool': 'refund', 'mcp.approval': 'required' },
              input_json_schema: { $schema: 'https://json-schema.org/draft/2020-12/schema', type: 'object' },
            },
            { name: 'internal' },
          ],
        },
      ],
    });
    expect(tools).toEqual([
      {
        name: 'refund',
        description: 'Refund an order.',
        inputSchema: { type: 'object' },
        approval: true,
        service: 'Payments',
        handler: 'refund',
      },
    ]);
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

const result = (text: string) => ({ role: 'toolResult', content: [{ type: 'text', text }], isError: false });

describe('the scripted model', () => {
  it('looks up a range of orders at once, and refunds', () => {
    expect(toolCalls(user('where are orders 101 to 103?'))).toEqual([
      ['lookup_order', { order: '101' }],
      ['lookup_order', { order: '102' }],
      ['lookup_order', { order: '103' }],
    ]);
    expect(toolCalls(user('refund 30 of order 103'))).toEqual([['refund', { order: '103', amount: 30 }]]);
  });

  it("reports the last round's results", () => {
    const reply = concierge({ messages: [user('…'), result('one'), result('two')] } as unknown as TranscriptContext);
    expect(reply.content).toEqual([{ type: 'text', text: 'one\ntwo' }]);
  });
});

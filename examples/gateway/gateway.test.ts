// The gateway's parts without a Restate server: the registry, the MCP
// endpoint over a fake gateway, and the scripted model. Against a server:
// demo.ts.
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { TranscriptContext } from '@earendil-works/pi-ai';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { concierge } from './agent.ts';
import { identity, toolsOf, type CallRequest } from './gateway.ts';
import { mcpHandler, type GatewayClient } from './mcp.ts';

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

describe('the MCP endpoint', () => {
  const calls: { req: CallRequest; idempotencyKey: string }[] = [];
  const gateway: GatewayClient = {
    tools: async (tenant) => [
      { name: `${tenant}_tool`, description: 'A tool.', inputSchema: {}, approval: false, service: 'S', handler: 'h' },
    ],
    call: async (req, idempotencyKey) => {
      calls.push({ req, idempotencyKey });
      return { content: [{ type: 'text', text: `ran ${req.name}` }] };
    },
  };
  let server: Server;
  let url: string;
  beforeAll(async () => {
    server = createServer((req, res) => void mcpHandler(gateway)(req, res));
    await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
    url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/mcp`;
  });
  afterAll(() => server.close());

  const post = (body: unknown, headers: Record<string, string> = { authorization: 'Bearer acme.alice' }) =>
    fetch(url, { method: 'POST', headers, body: JSON.stringify(body) });
  const rpc = async (method: string, params?: unknown) =>
    (await (await post({ jsonrpc: '2.0', id: 7, method, params })).json()) as Record<string, unknown>;

  it('initializes, and lists the tenant’s tools', async () => {
    expect(await rpc('initialize')).toMatchObject({ id: 7, result: { capabilities: { tools: {} } } });
    expect((await rpc('tools/list')).result).toEqual({
      tools: [{ name: 'acme_tool', description: 'A tool.', inputSchema: {} }],
    });
  });

  it("calls a tool for the token's user, with a key of the session and the request id", async () => {
    const res = await post(
      { jsonrpc: '2.0', id: 7, method: 'tools/call', params: { name: 'acme_tool', arguments: { a: 1 } } },
      { authorization: 'Bearer acme.alice', 'mcp-session-id': 's1' },
    );
    expect(((await res.json()) as { result: unknown }).result).toEqual({
      content: [{ type: 'text', text: 'ran acme_tool' }],
    });
    expect(calls.at(-1)).toEqual({
      req: { tenant: 'acme', user: 'alice', name: 'acme_tool', arguments: { a: 1 } },
      idempotencyKey: 'mcp:acme.alice:s1:7',
    });
  });

  it('accepts a notification; refuses an unknown method, a missing token, a GET', async () => {
    expect((await post({ jsonrpc: '2.0', method: 'notifications/initialized' })).status).toBe(202);
    expect(await rpc('resources/list')).toMatchObject({ error: { code: -32601 } });
    expect((await post({ jsonrpc: '2.0', id: 1, method: 'ping' }, {})).status).toBe(401);
    expect((await fetch(url)).status).toBe(405);
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

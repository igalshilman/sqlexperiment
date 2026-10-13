// The gateway's parts without a Restate server: the registry from the admin
// API, the MCP endpoint over a fake gateway, and the scripted concierge. The
// whole of it, against a server, is demo.ts.
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
              input_json_schema: {
                $schema: 'https://json-schema.org/draft/2020-12/schema',
                type: 'object',
                properties: { order: { type: 'string' } },
              },
            },
            { name: 'internal', metadata: {} },
          ],
        },
        { name: 'Ledger', handlers: [{ name: 'audit' }] },
      ],
    });
    expect(tools).toEqual([
      {
        name: 'refund',
        description: 'Refund an order.',
        inputSchema: { type: 'object', properties: { order: { type: 'string' } } },
        approval: true,
        service: 'Payments',
        handler: 'refund',
      },
    ]);
  });
});

describe('identity', () => {
  it('is the tenant and the user of `<tenant>.<user>`', () => {
    expect(identity('acme.alice')).toEqual({ tenant: 'acme', user: 'alice' });
    expect(identity('acme.a.b')).toEqual({ tenant: 'acme', user: 'a.b' });
    expect(identity('acme')).toEqual({ tenant: 'acme', user: 'anonymous' });
  });
});

describe('the MCP endpoint', () => {
  const calls: { req: CallRequest; idempotencyKey: string }[] = [];
  const gateway: GatewayClient = {
    tools: async (tenant) => [
      {
        name: `${tenant}_tool`,
        description: 'A tool.',
        inputSchema: { type: 'object' },
        approval: false,
        service: 'S',
        handler: 'h',
      },
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

  const post = (body: unknown, headers: Record<string, string> = { authorization: 'Bearer acme' }) =>
    fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body),
    });
  const rpc = async (method: string, params?: unknown, session?: string) => {
    const res = await post(
      { jsonrpc: '2.0', id: 7, method, params },
      { authorization: 'Bearer acme.alice', ...(session ? { 'mcp-session-id': session } : {}) },
    );
    return { session: res.headers.get('mcp-session-id')!, body: (await res.json()) as Record<string, unknown> };
  };

  it('initializes, and gives a session', async () => {
    const { session, body } = await rpc('initialize', { protocolVersion: '2025-06-18' });
    expect(session).toMatch(/^[0-9a-f-]{36}$/);
    expect(body).toMatchObject({
      id: 7,
      result: {
        protocolVersion: '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'restate-tool-gateway' },
      },
    });
  });

  it("lists the tenant's tools, as MCP lists them", async () => {
    const { body } = await rpc('tools/list');
    expect(body.result).toEqual({
      tools: [{ name: 'acme_tool', description: 'A tool.', inputSchema: { type: 'object' } }],
    });
  });

  it("calls a tool for the token's user, once per session and request id", async () => {
    calls.length = 0;
    const { body } = await rpc('tools/call', { name: 'acme_tool', arguments: { a: 1 } }, 's1');
    expect(body.result).toEqual({ content: [{ type: 'text', text: 'ran acme_tool' }] });
    expect(calls).toEqual([
      {
        req: { tenant: 'acme', user: 'alice', caller: 'mcp:s1', name: 'acme_tool', arguments: { a: 1 } },
        idempotencyKey: 'mcp:acme.alice:s1:7',
      },
    ]);
  });

  it('accepts a notification without an answer', async () => {
    const res = await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
    expect(res.status).toBe(202);
    expect(await res.text()).toBe('');
  });

  it('refuses an unknown method, a request without a token, and a GET', async () => {
    expect((await rpc('resources/list')).body).toMatchObject({ error: { code: -32601 } });
    expect((await post({ jsonrpc: '2.0', id: 1, method: 'ping' }, {})).status).toBe(401);
    expect((await fetch(url)).status).toBe(405);
  });
});

const user = (content: string) => ({ role: 'user', content, timestamp: 0 });
const calls = (context: unknown) =>
  concierge(context as TranscriptContext).content.flatMap((c) =>
    c.type === 'toolCall' ? [[c.name, c.arguments]] : [],
  );

const result = (text: string, isError: boolean) => ({
  role: 'toolResult',
  toolCallId: text,
  toolName: 't',
  content: [{ type: 'text', text }],
  isError,
  timestamp: 0,
});

describe('the concierge', () => {
  it('looks up a range of orders at once', () => {
    expect(calls({ messages: [user('where are orders 101 to 103?')] })).toEqual([
      ['lookup_order', { order: '101' }],
      ['lookup_order', { order: '102' }],
      ['lookup_order', { order: '103' }],
    ]);
  });

  it('refunds, and mails', () => {
    expect(calls({ messages: [user('refund 30 of order 103 because it broke')] })).toEqual([
      ['refund', { order: '103', amount: 30, reason: 'it broke' }],
    ]);
    expect(calls({ messages: [user('email the customer of order 7: hello')] })).toEqual([
      ['email_customer', { order: '7', text: 'hello' }],
    ]);
  });

  it("reports the last round's results", () => {
    const reply = concierge({
      messages: [user('…'), result('one', false), result('two', true)],
    } as unknown as TranscriptContext);
    expect(reply.content).toEqual([{ type: 'text', text: 'one\nfailed: two' }]);
  });
});

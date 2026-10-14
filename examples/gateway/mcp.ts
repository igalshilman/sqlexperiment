// The gateway as an MCP server (Streamable HTTP, JSON answers), for any MCP
// client:
//
//   claude mcp add --transport http restate-tools http://127.0.0.1:9400/mcp --header "Authorization: Bearer acme.alice"
//
// A thin shim over Restate's ingress; everything else happens in the gateway
// (gateway.ts). The bearer token is `<tenant>.<user>`. `tools/list` is
// the tenant's Gateway.tools; `tools/call` its Gateway.call, with an idempotency key made
// from the token, the MCP session and the JSON-RPC id: a call sent again
// gets the first one's result.
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { identity, type CallRequest, type Tool, type ToolResult } from './gateway.ts';

/** The two handlers of a tenant's gateway. */
export interface GatewayClient {
  tools(tenant: string): Promise<Tool[]>;
  call(tenant: string, req: CallRequest, idempotencyKey: string): Promise<ToolResult>;
}

/** The gateway through Restate's ingress. */
export function ingressGateway(ingress = 'http://127.0.0.1:8080'): GatewayClient {
  const post = async <T>(path: string, body: unknown, headers: Record<string, string> = {}) => {
    const res = await fetch(`${ingress}/Gateway/${path}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`Gateway/${path}: ${res.status} ${await res.text()}`);
    return (await res.json()) as T;
  };
  return {
    tools: (tenant) => post(`${encodeURIComponent(tenant)}/tools`, {}),
    call: (tenant, req, idempotencyKey) =>
      post(`${encodeURIComponent(tenant)}/call`, req, { 'idempotency-key': idempotencyKey }),
  };
}

interface Request {
  id?: string | number;
  method: string;
  params?: Record<string, unknown>;
}

/** The result of request `req` of `token` in `session`, or null for a method there is none of. */
async function answer(gateway: GatewayClient, token: string, session: string, req: Request) {
  const { tenant, user } = identity(token);
  switch (req.method) {
    case 'initialize':
      return {
        protocolVersion: '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'restate-tool-gateway', version: '0.1.0' },
      };
    case 'ping':
      return {};
    case 'tools/list':
      return {
        tools: (await gateway.tools(tenant)).map(({ name, description, inputSchema }) => ({
          name,
          description,
          inputSchema,
        })),
      };
    case 'tools/call':
      return gateway.call(
        tenant,
        {
          user,
          name: String(req.params?.name),
          arguments: (req.params?.arguments ?? {}) as Record<string, unknown>,
        },
        `mcp:${token}:${session}:${String(req.id)}`,
      );
    default:
      return null;
  }
}

/** The MCP endpoint, at `/mcp`, over `gateway`. */
export function mcpHandler(gateway: GatewayClient) {
  return async (req: IncomingMessage, res: ServerResponse) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1];
    const status = req.url !== '/mcp' ? 404 : req.method !== 'POST' ? 405 : token ? 200 : 401;
    if (status !== 200) {
      res.writeHead(status).end();
      return;
    }
    const message = JSON.parse(body) as Request;
    const asked = req.headers['mcp-session-id'];
    const session = typeof asked === 'string' ? asked : randomUUID();
    // A notification: nothing to answer.
    if (message.id === undefined) {
      res.writeHead(202, { 'mcp-session-id': session }).end();
      return;
    }
    const reply = await answer(gateway, token!, session, message).then(
      (result) => (result ? { result } : { error: { code: -32601, message: `no method ${message.method}` } }),
      (e: unknown) => ({ error: { code: -32603, message: String(e) } }),
    );
    res.writeHead(200, { 'content-type': 'application/json', 'mcp-session-id': session });
    res.end(JSON.stringify({ jsonrpc: '2.0', id: message.id, ...reply }));
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  createServer((req, res) => void mcpHandler(ingressGateway())(req, res)).listen(9400, '127.0.0.1', () =>
    console.error('MCP on http://127.0.0.1:9400/mcp'),
  );
}

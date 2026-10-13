// The gateway as an MCP server, for any MCP client: Claude Code, the MCP
// Inspector, another agent. Streamable HTTP, answering each request with JSON.
//
//   claude mcp add --transport http restate-tools http://127.0.0.1:9400/mcp --header "Authorization: Bearer acme.alice"
//
// A thin shim: it speaks MCP's JSON-RPC and calls the gateway through
// Restate's ingress, where everything else happens (gateway.ts). The bearer
// token is the tenant and the user, `<tenant>.<user>` (or `<tenant>`, for
// an anonymous user). `tools/list` is Gateway.tools; `tools/call` is
// Gateway.call with an idempotency key made from the MCP session and the
// request's JSON-RPC id, so a client that sends a call again (a lost
// response, a reconnect) gets the first call's result, not a second refund.
//
// Environment: MCP_PORT (default 9400) and RESTATE_INGRESS_URL (default
// http://127.0.0.1:8080).
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { identity, type CallRequest, type Tool, type ToolResult } from './gateway.ts';

const PROTOCOL_VERSIONS = ['2025-06-18', '2025-03-26', '2024-11-05'];

interface JsonRpcRequest {
  jsonrpc: '2.0';
  id?: string | number;
  method: string;
  params?: Record<string, unknown>;
}

/** The gateway's two handlers, as the MCP server calls them. */
export interface GatewayClient {
  tools(tenant: string): Promise<Tool[]>;
  call(req: CallRequest, idempotencyKey: string): Promise<ToolResult>;
}

/** The gateway through Restate's ingress at `ingress`. */
export function ingressGateway(ingress: string): GatewayClient {
  const post = async <T>(handler: string, body: unknown, headers: Record<string, string> = {}) => {
    const res = await fetch(`${ingress}/Gateway/${handler}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`Gateway/${handler}: ${res.status} ${await res.text()}`);
    return (await res.json()) as T;
  };
  return {
    tools: (tenant) => post('tools', { tenant }),
    call: (req, idempotencyKey) => post('call', req, { 'idempotency-key': idempotencyKey }),
  };
}

/** A JSON-RPC error. */
class RpcError extends Error {
  code: number;
  constructor(code: number, message: string) {
    super(message);
    this.code = code;
  }
}

/** The answer to one MCP request of `user` of `tenant` in `session`. */
export async function answer(
  gateway: GatewayClient,
  { tenant, user }: { tenant: string; user: string },
  session: string,
  req: JsonRpcRequest,
) {
  switch (req.method) {
    case 'initialize': {
      const asked = req.params?.protocolVersion;
      return {
        protocolVersion: typeof asked === 'string' && PROTOCOL_VERSIONS.includes(asked) ? asked : PROTOCOL_VERSIONS[0],
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: 'restate-tool-gateway', version: '0.1.0' },
        instructions: `Tools of tenant ${tenant}, through a Restate gateway: each call is approved, limited and audited.`,
      };
    }
    case 'ping':
      return {};
    case 'tools/list': {
      const tools = await gateway.tools(tenant);
      return { tools: tools.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })) };
    }
    case 'tools/call': {
      const name = req.params?.name;
      if (typeof name !== 'string') throw new RpcError(-32602, 'tools/call needs a tool name');
      const args = (req.params?.arguments ?? {}) as Record<string, unknown>;
      return gateway.call(
        { tenant, user, caller: `mcp:${session}`, name, arguments: args },
        `mcp:${tenant}.${user}:${session}:${String(req.id)}`,
      );
    }
    default:
      throw new RpcError(-32601, `no method ${req.method}`);
  }
}

const read = async (req: IncomingMessage) => {
  let text = '';
  for await (const chunk of req) text += chunk;
  return text;
};

/** The MCP endpoint's request handler, over `gateway`. */
export function mcpHandler(gateway: GatewayClient) {
  return async (req: IncomingMessage, res: ServerResponse) => {
    const json = (status: number, value: unknown, headers: Record<string, string> = {}) => {
      res.writeHead(status, { 'content-type': 'application/json', ...headers });
      res.end(JSON.stringify(value));
    };
    const url = new URL(req.url ?? '/', 'http://mcp');
    if (url.pathname !== '/mcp') return json(404, { error: 'not found' });
    // No stream of server messages, and sessions end on their own.
    if (req.method !== 'POST') return json(405, { error: 'POST only' }, { allow: 'POST' });
    const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1];
    if (!token) return json(401, { error: 'a bearer token (`<tenant>.<user>`) is required' });
    let message: JsonRpcRequest;
    try {
      message = JSON.parse(await read(req)) as JsonRpcRequest;
    } catch {
      return json(400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } });
    }
    const asked = req.headers['mcp-session-id'];
    const session = typeof asked === 'string' ? asked : randomUUID();
    const headers = { 'mcp-session-id': session };
    // A notification or a response: accepted, nothing to answer.
    if (message.id === undefined || !message.method) {
      res.writeHead(202, headers);
      return res.end();
    }
    try {
      const result = await answer(gateway, identity(token), session, message);
      json(200, { jsonrpc: '2.0', id: message.id, result }, headers);
    } catch (e) {
      const code = e instanceof RpcError ? e.code : -32603;
      json(200, { jsonrpc: '2.0', id: message.id, error: { code, message: (e as Error).message } }, headers);
    }
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const port = Number(process.env.MCP_PORT ?? 9400);
  const gateway = ingressGateway(process.env.RESTATE_INGRESS_URL ?? 'http://127.0.0.1:8080');
  createServer((req, res) => void mcpHandler(gateway)(req, res)).listen(port, '127.0.0.1', () =>
    console.error(`MCP on http://127.0.0.1:${port}/mcp`),
  );
}

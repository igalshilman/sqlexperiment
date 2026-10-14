// The tool gateway, as a demo that checks itself: a Restate server, the fake
// outside world (upstream.ts), the tools, the gateway and the agents, each in
// its own process, the agents crashing at random state requests.
//
// 1. Tools are just handlers: each tenant's tools, found by their metadata
//    and filtered by its policy.
// 2. Crash anytime: 12 lookups at once while the agents crash; the order
//    system 429s what it cannot serve, the tools retry, and it serves each
//    order once.
// 3. Limits without code: a rule per tenant (`*/lookup_order` at 2) queues
//    the calls over it, and another tenant's pass; a rule per user
//    (`*/lookup_order/*` at 1) runs one user's burst one at a time, and
//    another user of the tenant does not wait for it.
// 4. Human in the loop: a refund waits for a manager; the agents' process
//    is killed meanwhile; the manager approves; one refund.
// 5. Any client: an MCP client gets the same gateway, its policy, and its
//    idempotency: a call sent twice is served once.
//
//   RESTATE_SERVER=/path/to/restate-server node gateway/demo.ts
//
// Ports 8080, 9070, 9300, 9400 and 9411 to 9413 must be free.
import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtempSync, openSync } from 'node:fs';
import { createServer } from 'node:http';
import { connect } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { CAPACITY, startUpstream } from './upstream.ts';
import { ingressGateway, mcpHandler } from './mcp.ts';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) throw new Error('RESTATE_SERVER is not set');
const here = fileURLToPath(new URL('.', import.meta.url));
const dir = mkdtempSync(join(tmpdir(), 'tool-gateway-'));
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const act = (title: string, what: string) => console.log(`\n${title}\n${what}\n`);
const say = (s: string) => console.log(`  ${s}`);

// ---- processes ----

const children = new Set<ChildProcess>();
let stopping = false;
process.on('exit', () => {
  stopping = true;
  for (const child of children) child.kill('SIGKILL');
});
process.on('SIGINT', () => process.exit(130));
const log = (name: string) => openSync(join(dir, `${name}.log`), 'a');

/** Run `command`, and start it again whenever it dies. */
function supervise(name: string, command: string, args: string[], env: Record<string, string> = {}) {
  const service = { restarts: 0, child: undefined as ChildProcess | undefined };
  const start = () => {
    const child = spawn(command, args, { env: { ...process.env, ...env }, stdio: ['ignore', log(name), log(name)] });
    children.add((service.child = child));
    child.on('exit', () => {
      children.delete(child);
      if (stopping) return;
      service.restarts++;
      setTimeout(start, 100);
    });
  };
  start();
  return service;
}

const listening = async (port: number) => {
  for (let i = 0; i < 200; i++) {
    const open = await new Promise<boolean>((resolve) => {
      const socket = connect(port, '127.0.0.1', () => (socket.end(), resolve(true)));
      socket.on('error', () => resolve(false));
    });
    if (open) return;
    await sleep(100);
  }
  throw new Error(`nothing listens on ${port}`);
};

const post = async <T>(url: string, body: unknown, headers: Record<string, string> = {}): Promise<T> => {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json', ...headers },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${await res.text()}`);
  return (await res.json()) as T;
};
const ingress = <T>(path: string, body: unknown = {}) => post<T>(`http://127.0.0.1:8080${path}`, body);
const query = async <T>(sql: string) => (await post<{ rows: T[] }>('http://127.0.0.1:9070/query', { query: sql })).rows;

/** Prompt agent `key` once, by prompt id. */
const prompt = async (key: string, id: string, text: string) =>
  (
    await post<{ answer: string }>(
      `http://127.0.0.1:8080/Assistant/${key}/prompt`,
      { id, text },
      { 'idempotency-key': id },
    )
  ).answer;

/** The limit rules: per tenant (2, or none), and per user (or none). */
const rules = async (perTenant: boolean, perUser?: number) => {
  const res = await fetch('http://127.0.0.1:9070/limits/rules', {
    method: 'PUT',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify([
      { pattern: '*/lookup_order', limits: { concurrency: CAPACITY }, disabled: !perTenant },
      { pattern: '*/lookup_order/*', limits: { concurrency: perUser ?? 1 }, disabled: perUser === undefined },
    ]),
  });
  assert.ok(res.ok, await res.text());
};

/** The MCP session of each token. */
const sessions = new Map<string, string>();

/** One MCP request with `token`, in its session; `id` undefined is a notification. */
const mcp = async (token: string, method: string, params?: unknown, id: number | undefined = 1) => {
  const session = sessions.get(token);
  const res = await fetch('http://127.0.0.1:9400/mcp', {
    method: 'POST',
    headers: {
      authorization: `Bearer ${token}`,
      accept: 'application/json, text/event-stream',
      ...(session ? { 'mcp-session-id': session } : {}),
    },
    body: JSON.stringify({ jsonrpc: '2.0', id, method, params }),
  });
  sessions.set(token, res.headers.get('mcp-session-id')!);
  return res.status === 202 ? undefined : ((await res.json()) as { result: Record<string, unknown> }).result;
};

/** Print the lookups' queues, as they change, until `work` settles: per tenant, running (of its limit), and per user. */
async function watched<T>(work: Promise<T>): Promise<T> {
  const state = { done: false };
  const watching = (async () => {
    let last = '';
    while (!state.done) {
      const queues = await query<{ scope: string; limit_key: string; num_inbox: number; num_running: number }>(
        `SELECT scope, limit_key, num_inbox, num_running FROM sys_vqueue_meta
          WHERE limit_key LIKE 'lookup_order/%' AND num_inbox + num_running > 0 ORDER BY scope, limit_key`,
      );
      const limits = await query<{ scope: string; l2?: string; concurrency_limit?: number }>(
        "SELECT scope, l2, concurrency_limit FROM sys_user_limits WHERE l1 = 'lookup_order'",
      );
      const of = (n: number, scope: string, l2?: string) => {
        const limit = limits.find((l) => l.scope === scope && l.l2 === l2)?.concurrency_limit;
        return limit ? `${n}/${limit}` : `${n}`;
      };
      const line = [...new Set(queues.map((q) => q.scope))]
        .map((scope) => {
          const mine = queues.filter((q) => q.scope === scope);
          const users = mine.map((q) => {
            const user = q.limit_key.split('/')[1]!;
            return `${user} ${of(q.num_running, scope, user)}${q.num_inbox ? ` +${q.num_inbox} queued` : ''}`;
          });
          const running = mine.reduce((n, q) => n + q.num_running, 0);
          return `${scope} ${of(running, scope)} (${users.join(', ')})`;
        })
        .join('  |  ');
      if (line && line !== last) say(`[queues] ${line}`);
      last = line;
      await sleep(150);
    }
  })();
  try {
    return await work;
  } finally {
    state.done = true;
    await watching;
  }
}

// ---- the cast ----

console.log(`logs in ${dir}`);
// Scopes and limits, on a 1.7 server (on by default from 1.8).
supervise('server', SERVER, ['--base-dir', join(dir, 'server')], {
  RESTATE_BIND_IP: '127.0.0.1',
  RESTATE_LISTEN_MODE: 'tcp',
  RESTATE_EXPERIMENTAL_ENABLE_VQUEUES: 'true',
  RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7: 'true',
});
const upstream = startUpstream(9300);
await upstream.ready;
const { orders, served, refunds } = upstream.stats;
const mcpServer = createServer((req, res) => void mcpHandler(ingressGateway())(req, res));
await new Promise<void>((r) => mcpServer.listen(9400, '127.0.0.1', r));
for (const server of [upstream.server, mcpServer]) server.unref();
const node = (name: string, file: string, port: number, env: Record<string, string> = {}) =>
  supervise(name, process.execPath, [join(here, file)], { PORT: String(port), ...env });
node('tools', 'tools.ts', 9411);
node('gateway', 'gateway.ts', 9412);
const agents = node('agents', 'agent.ts', 9413, { ABORT_PER_MILLE: process.env.AGENT_ABORT_PER_MILLE ?? '2' });
await listening(8080);
for (const port of [9070, 9411, 9412, 9413]) await listening(port);
for (const port of [9411, 9412, 9413])
  await post('http://127.0.0.1:9070/deployments', { uri: `http://127.0.0.1:${port}` });
await rules(false);

// ---- 1. tools are just handlers ----

act('1. Tools are just handlers', "  Each tenant's tools: the handlers whose metadata names a tool, per its policy.");
const toolsOf = async (tenant: string) =>
  (await ingress<{ name: string }[]>(`/Gateway/${tenant}/tools`)).map((t) => t.name).toSorted();
const acmeTools = await toolsOf('acme');
const globexTools = await toolsOf('globex');
say(`acme: ${acmeTools.join(', ')}`);
say(`globex: ${globexTools.join(', ')}`);
assert.deepEqual(acmeTools, ['lookup_order', 'refund']);
assert.deepEqual(globexTools, ['lookup_order']);

// ---- 2. crash anytime ----

act(
  '2. Crash anytime',
  "  alice (acme) asks about 12 orders: 12 lookups at once, while the agents' process crashes at random.",
);
let start = performance.now();
const restarts = agents.restarts;
await watched(prompt('acme.alice', 'p1', 'where are orders 101 to 112?'));
const burst = Array.from({ length: 12 }, (_, i) => String(101 + i));
assert.ok(burst.every((o) => served[o] === 1));
say(
  `answered in ${Math.round(performance.now() - start)} ms; the agents' process crashed ` +
    `${agents.restarts - restarts} times, the order system refused ${orders.acme!.refused} lookups with a 429, ` +
    'and served each order once',
);

// ---- 3. limits without code ----

act(
  '3. Limits without code',
  `  restate rules set '*/lookup_order' --concurrency ${CAPACITY}\n` +
    '  alice asks about 16 orders, and dan (globex) about 3.',
);
await rules(true);
for (const seen of Object.values(orders)) Object.assign(seen, { peak: 0, refused: 0, first: 0 });
start = performance.now();
await watched(
  Promise.all([
    prompt('acme.alice', 'p2', 'where are orders 113 to 128?'),
    sleep(300).then(() => prompt('globex.dan', 'p3', 'where are orders 201 to 203?')),
  ]),
);
say(
  `answered in ${Math.round(performance.now() - start)} ms; at most ${orders.acme!.peak} of acme's and ` +
    `${orders.globex!.peak} of globex's at once; ${orders.acme!.refused + orders.globex!.refused} refused`,
);
assert.equal(orders.acme!.refused + orders.globex!.refused, 0);
// globex's lookups do not wait for acme's: the first came in while acme's still did.
assert.ok(orders['globex/dan']!.first < orders['acme/alice']!.last);

console.log(
  "\n  restate rules set '*/lookup_order/*' --concurrency 1\n" +
    '  alice asks about 6 orders; while they queue, erin (acme too) asks about 2.\n',
);
await rules(true, 1);
for (const seen of Object.values(orders)) Object.assign(seen, { peak: 0, first: 0 });
/** Until some of alice's lookups queue. */
const aliceQueued = async () => {
  for (;;) {
    const [q] = await query<{ n: number }>(
      "SELECT max(num_inbox) AS n FROM sys_vqueue_meta WHERE scope = 'acme' AND limit_key = 'lookup_order/alice'",
    );
    if ((q?.n ?? 0) >= 3) return;
    await sleep(50);
  }
};
await watched(
  Promise.all([
    prompt('acme.alice', 'p4', 'where are orders 131 to 136?'),
    aliceQueued().then(() => prompt('acme.erin', 'p5', 'where are orders 141 to 142?')),
  ]),
);
say(`at most ${orders['acme/alice']!.peak} of alice's and ${orders['acme/erin']!.peak} of erin's at once`);
assert.equal(orders['acme/alice']!.peak, 1);
// erin's lookups do not wait for alice's.
assert.ok(orders['acme/erin']!.first < orders['acme/alice']!.last);

// ---- 4. human in the loop ----

act('4. Human in the loop', '  bob (acme) asks for a refund, which a manager approves.');
const refund = prompt('acme.bob', 'p6', 'refund 30 of order 103');
let held: { awakeable: string; name: string; arguments: unknown }[] = [];
while (held.length === 0) held = await sleep(200).then(() => ingress('/Gateway/acme/pending'));
say(`held: ${held[0]!.name} ${JSON.stringify(held[0]!.arguments)}`);
await sleep(1500);
say("kill -9 the agents' process, then approve: curl localhost:8080/Gateway/acme/decide --json '{…}'");
agents.child?.kill('SIGKILL');
await sleep(1000);
await ingress('/Gateway/acme/decide', { awakeable: held[0]!.awakeable, approved: true });
say(`bob's answer: ${await refund}; the payment provider made ${refunds.length} refund`);
assert.equal(refunds.length, 1);

// ---- 5. any client ----

act('5. Any client', "  An MCP client with dan's token at globex (`globex.dan`), as Claude Code would be.");
await mcp('globex.dan', 'initialize', { protocolVersion: '2025-06-18', capabilities: {} });
await mcp('globex.dan', 'notifications/initialized', undefined, undefined);
const listed = (await mcp('globex.dan', 'tools/list'))!.tools as { name: string }[];
say(`tools/list: ${listed.map((t) => t.name).join(', ')}`);
const denied = (await mcp('globex.dan', 'tools/call', { name: 'refund', arguments: { order: '204', amount: 5 } }))!;
say(`tools/call refund: ${JSON.stringify(denied)}`);
const lookup = { name: 'lookup_order', arguments: { order: '301' } };
const [first, again] = await Promise.all([1, 2].map(() => mcp('globex.dan', 'tools/call', lookup, 7)));
say(`tools/call lookup_order, sent twice: ${JSON.stringify(first)}, served ${served['301']} time`);
assert.deepEqual(
  listed.map((t) => t.name),
  ['lookup_order'],
);
assert.equal(denied.isError, true);
assert.deepEqual(first, again);
assert.equal(served['301'], 1);

console.log(`\npassed; the agents' process restarted ${agents.restarts} times`);
process.exit(0);

// The tool gateway, as a demo that checks itself: a Restate server, the fake
// outside world (upstream.ts), the tools, the gateway and the agents, each in
// its own process, the agents crashing at random state requests.
//
// 1. A burst without a limit: 12 lookups at once; the order system, which
//    serves 2 at a time per tenant, 429s the rest, and the tools retry.
// 2. A limit per tenant (`*/lookup_order` at 2): the calls over it wait in
//    the tenant's queue, none is refused, and another tenant's pass.
// 3. A limit per user (`*/lookup_order/*` at 1): one user's burst runs one
//    at a time, and another user of the tenant does not wait for it.
// 4. Approval: a refund waits for a manager; the agents' process is killed
//    meanwhile; the manager approves; one refund.
// 5. MCP: a client's tools/list and tools/call, with the tenant's policy.
// 6. What ran: the server's invocations, one tool invocation per tool call.
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

/** One MCP request with `token`; `id` undefined is a notification. */
const mcp = async (token: string, method: string, params?: unknown, id: number | undefined = 1) => {
  const res = await fetch('http://127.0.0.1:9400/mcp', {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, accept: 'application/json, text/event-stream' },
    body: JSON.stringify({ jsonrpc: '2.0', id, method, params }),
  });
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
const { orders, refunds } = upstream.stats;
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
const tools = (await ingress<{ name: string }[]>('/Gateway/acme/tools')).map((t) => t.name);
say(`acme's tools, from the handlers' metadata: ${tools.join(', ')}`);

// ---- 1. without a limit ----

act('1. A burst, without a limit', '  alice (acme) asks about 12 orders: 12 lookups at once.');
let start = performance.now();
await watched(prompt('acme.alice', 'p1', 'where are orders 101 to 112?'));
say(
  `answered in ${Math.round(performance.now() - start)} ms; the order system refused ${orders.acme!.refused} with a 429`,
);
assert.ok(orders.acme!.refused > 0);

// ---- 2. per tenant ----

act(
  '2. A limit per tenant',
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
assert.ok(orders.acme!.peak <= CAPACITY && orders.globex!.peak <= CAPACITY);
// globex's lookups do not wait for acme's: the first came in while acme's still did.
assert.ok(orders['globex/dan']!.first < orders['acme/alice']!.last);

// ---- 3. per user ----

act(
  '3. A limit per user',
  "  restate rules set '*/lookup_order/*' --concurrency 1\n" +
    '  alice asks about 6 orders; while they queue, erin (acme too) looks up 2 from an MCP client.',
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
    aliceQueued().then(() =>
      Promise.all(
        ['141', '142'].map((order, i) =>
          mcp('acme.erin', 'tools/call', { name: 'lookup_order', arguments: { order } }, i),
        ),
      ),
    ),
  ]),
);
say(`at most ${orders['acme/alice']!.peak} of alice's and ${orders['acme/erin']!.peak} of erin's at once`);
assert.equal(orders['acme/alice']!.peak, 1);
assert.equal(orders['acme/erin']!.peak, 1);
// erin's lookups do not wait for alice's.
assert.ok(orders['acme/erin']!.first < orders['acme/alice']!.last);

// ---- 4. approval ----

act('4. Human in the loop', '  bob (acme) asks for a refund, which a manager approves.');
const refund = prompt('acme.bob', 'p5', 'refund 30 of order 103');
let held: { awakeable: string; tool: string; arguments: unknown }[] = [];
while (held.length === 0) held = await sleep(200).then(() => ingress('/Gateway/acme/pending'));
say(`held: ${held[0]!.tool} ${JSON.stringify(held[0]!.arguments)}`);
await sleep(1500);
const waiting = await query<{ target: string; status: string }>(
  "SELECT target, status FROM sys_invocation WHERE target_service_name IN ('Assistant', 'Gateway') AND status != 'completed'",
);
say(`meanwhile: ${waiting.map((w) => `${w.target} ${w.status}`).join(', ')}`);
say("kill -9 the agents' process, then approve: curl localhost:8080/Gateway/acme/decide --json '{…}'");
agents.child?.kill('SIGKILL');
await sleep(1000);
await ingress('/Gateway/acme/decide', { awakeable: held[0]!.awakeable, approved: true });
say(`bob's answer: ${await refund}; the payment provider made ${refunds.length} refund`);
assert.equal(refunds.length, 1);

// ---- 5. MCP ----

act('5. MCP', "  A client with dan's token at globex (`globex.dan`), as Claude Code would be.");
await mcp('globex.dan', 'initialize', { protocolVersion: '2025-06-18', capabilities: {} });
await mcp('globex.dan', 'notifications/initialized', undefined, undefined);
const listed = (await mcp('globex.dan', 'tools/list'))!.tools as { name: string }[];
say(`tools/list: ${listed.map((t) => t.name).join(', ')}`);
const denied = (await mcp('globex.dan', 'tools/call', { name: 'refund', arguments: { order: '204', amount: 5 } }))!;
say(`tools/call refund: ${JSON.stringify(denied)}`);
assert.deepEqual(
  listed.map((t) => t.name),
  ['lookup_order'],
);
assert.equal(denied.isError, true);

// ---- 6. what ran ----

act('6. What ran', "  The server's invocations, by SQL on the admin API: each tenant's gateway calls, and the tools'.");
const invocations = await query<{ target: string; key: string | null; n: number }>(
  `SELECT target, target_service_key AS key, count(*) AS n FROM sys_invocation
    WHERE target IN ('Gateway/call', 'Orders/lookup', 'Payments/refund') GROUP BY target, key ORDER BY target, key`,
);
console.table(invocations);
// One invocation per tool call, however often the agents crashed.
assert.deepEqual(
  invocations.map((i) => [i.target, i.key, i.n]),
  [
    ['Gateway/call', 'acme', 12 + 16 + 6 + 2 + 1],
    ['Gateway/call', 'globex', 3 + 1],
    ['Orders/lookup', null, 12 + 16 + 6 + 2 + 3],
    ['Payments/refund', null, 1],
  ],
);
console.log(`\npassed; the agents' process restarted ${agents.restarts} times`);
process.exit(0);

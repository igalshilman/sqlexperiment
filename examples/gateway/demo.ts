// The tool gateway, as a demo that checks itself: a Restate server, the fake
// outside world (upstream.ts), the tools, the gateway and the agents, each in
// its own process, the agents crashing at random state requests. Six acts:
//
// 1. A burst, without a limit: an agent looks up 12 orders at once, and the
//    order system, which serves 2 at a time per tenant, refuses the rest with
//    429s: the tools retry them, and the burst is slow and noisy.
// 2. The same burst, with a limit rule (`*/lookup_order` at 2), set while
//    everything runs: the calls over the limit wait in the tenant's scope,
//    the order system never refuses one, and another tenant's lookups, in
//    their own scope, pass the queue.
// 3. Users within a tenant: the limit key is `<tool>/<user>`, so each user's
//    calls queue apart and take turns: erin's lookups, from an MCP client,
//    pass alice's queued burst. Then a rule on `*/lookup_order/*` caps each
//    user at one at a time, so no user takes the whole tenant's limit.
// 4. Human in the loop: a refund waits for a manager; the agent is killed
//    while it waits; the manager approves; the money moves once.
// 5. MCP: an MCP client (globex's token) lists its tools, calls one, is
//    refused another, and sends a call twice: one mail.
// 6. The audit log: the tenant's SQL table of every call.
//
//   RESTATE_SERVER=/path/to/restate-server node gateway/demo.ts
//
// Environment: AGENT_ABORT_PER_MILLE (default 2), and the server's ports
// 8080 and 9070 must be free.
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
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const UPSTREAM_PORT = 9300;
const MCP_PORT = 9400;
const AGENT_PER_MILLE = Number(process.env.AGENT_ABORT_PER_MILLE ?? 2);
const here = fileURLToPath(new URL('.', import.meta.url));
const dir = mkdtempSync(join(tmpdir(), 'tool-gateway-'));
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

const bold = (s: string) => (process.stdout.isTTY ? `\x1b[1m${s}\x1b[0m` : s);
const act = (title: string, what: string) => console.log(`\n${bold(title)}\n${what}\n`);
const say = (s: string) => console.log(`  ${s}`);

// ---- processes -------------------------------------------------------------

const children = new Set<ChildProcess>();
let stopping = false;
const stopAll = () => {
  stopping = true;
  for (const child of children) child.kill('SIGKILL');
};
process.on('exit', stopAll);
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP'] as const) {
  process.on(signal, () => {
    stopAll();
    process.exit(130);
  });
}

const logFile = (name: string) => openSync(join(dir, `${name}.log`), 'a');

/** A service process, restarted whenever it dies, until the demo ends. */
function supervise(name: string, file: string, port: number, env: Record<string, string> = {}) {
  const service = { restarts: 0, child: undefined as ChildProcess | undefined };
  const start = () => {
    const log = logFile(name);
    const child = spawn(process.execPath, [join(here, file)], {
      env: { ...process.env, PORT: String(port), ...env },
      stdio: ['ignore', log, log],
    });
    children.add(child);
    service.child = child;
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

/** Wait until something listens on `port`. */
const waitFor = async (port: number) => {
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

const admin = async (method: string, path: string, body?: unknown): Promise<unknown> => {
  const res = await fetch(`${ADMIN}${path}`, {
    method,
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${method} ${path}: ${res.status} ${await res.text()}`);
  return res.json();
};

const query = async <T>(sql: string) => ((await admin('POST', '/query', { query: sql })) as { rows: T[] }).rows;

const ingress = async <T>(path: string, body: unknown, headers: Record<string, string> = {}) => {
  const res = await fetch(`${INGRESS}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
  return res.json() as Promise<T>;
};

/** Prompt the agent `key` once, by prompt id. */
const prompt = (key: string, id: string, text: string) =>
  ingress<{ answer: string }>(`/Assistant/${key}/prompt`, { id, text }, { 'idempotency-key': id }).then(
    (r) => r.answer,
  );

// The limit rules: on each tenant's lookups (`*/lookup_order`), disabled or
// not, and on each user's (`*/lookup_order/*`), if `perUser` is set.
const limitRules = ({ disabled = false, perUser }: { disabled?: boolean; perUser?: number }) =>
  admin('PUT', '/limits/rules', [
    {
      pattern: '*/lookup_order',
      limits: { concurrency: CAPACITY },
      description: 'the order system serves 2 requests at once per API key',
      disabled,
    },
    {
      pattern: '*/lookup_order/*',
      limits: perUser === undefined ? {} : { concurrency: perUser },
      description: "no user takes the whole of a tenant's limit",
      disabled: perUser === undefined,
    },
  ]);

/**
 * Print what the limiter holds, as it changes, until `done`: per tenant, its
 * lookups running (of its limit), and per user, the user's running (of the
 * user's limit) and queued.
 */
async function watch(done: () => boolean, seen: Queue[]) {
  let last = '';
  while (!done()) {
    const limits = await query<{ scope: string; l2?: string; usage: number; concurrency_limit?: number }>(
      "SELECT scope, l2, usage, concurrency_limit FROM sys_user_limits WHERE l1 = 'lookup_order'",
    ).catch(() => []);
    const queues = await query<{ scope: string; limit_key: string; num_inbox: number; num_running: number }>(
      "SELECT scope, limit_key, num_inbox, num_running FROM sys_vqueue_meta WHERE limit_key LIKE 'lookup_order/%' ORDER BY scope, limit_key",
    ).catch(() => []);
    const active = queues.filter((q) => q.num_inbox + q.num_running > 0);
    const at = performance.now();
    seen.push(
      ...active.map((q) => ({
        at,
        scope: q.scope,
        user: q.limit_key.slice('lookup_order/'.length),
        running: q.num_running,
        queued: q.num_inbox,
      })),
    );
    const line = [...new Set(active.map((q) => q.scope))]
      .map((scope) => {
        const tenant = limits.find((l) => l.scope === scope && !l.l2);
        const running = active.filter((q) => q.scope === scope).reduce((n, q) => n + q.num_running, 0);
        const users = active
          .filter((q) => q.scope === scope)
          .map((q) => {
            const user = q.limit_key.slice('lookup_order/'.length);
            const limit = limits.find((l) => l.scope === scope && l.l2 === user)?.concurrency_limit;
            return `${user} ${of(q.num_running, limit)}${q.num_inbox ? ` +${q.num_inbox} queued` : ''}`;
          });
        return `${scope} ${of(running, tenant?.concurrency_limit)} (${users.join(', ')})`;
      })
      .join('   |   ');
    if (line && line !== last) say(`[limiter] ${line}`);
    last = line;
    await sleep(150);
  }
}

/** `n`, of `limit` if there is one. */
const of = (n: number, limit?: number) => (limit ? `${n}/${limit}` : `${n}`);

/** A user's queue, as the watcher saw it. */
interface Queue {
  at: number;
  scope: string;
  user: string;
  running: number;
  queued: number;
}

/** Run `work`, watching the limiter meanwhile; `seen` gets what it saw. */
async function watched<T>(work: Promise<T>, seen: Queue[] = []): Promise<T> {
  let done = false;
  const watching = watch(() => done, seen);
  try {
    return await work;
  } finally {
    done = true;
    await watching;
  }
}

const timed = async <T>(work: Promise<T>) => {
  const start = performance.now();
  const value = await work;
  return { value, ms: Math.round(performance.now() - start) };
};

// ---- a minimal MCP client --------------------------------------------------

function mcpClient(token: string) {
  let session: string | undefined;
  let id = 0;
  /** Send a request, or a notification with a `requestId` of null. */
  const send = async (method: string, params?: unknown, requestId: number | null = ++id) => {
    const res = await fetch(`http://127.0.0.1:${MCP_PORT}/mcp`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        accept: 'application/json, text/event-stream',
        authorization: `Bearer ${token}`,
        'mcp-protocol-version': '2025-06-18',
        ...(session ? { 'mcp-session-id': session } : {}),
      },
      body: JSON.stringify({ jsonrpc: '2.0', ...(requestId === null ? {} : { id: requestId }), method, params }),
    });
    session = res.headers.get('mcp-session-id') ?? session;
    if (res.status === 202) return undefined;
    const reply = (await res.json()) as { result?: unknown; error?: { message: string } };
    if (reply.error) throw new Error(reply.error.message);
    return reply.result;
  };
  return {
    send,
    /** Call a tool with JSON-RPC id `requestId`: the same id, sent again, is the same call. */
    call: (name: string, args: unknown, requestId = ++id) =>
      send('tools/call', { name, arguments: args }, requestId) as Promise<{
        content: { text: string }[];
        isError?: boolean;
      }>,
  };
}

// ---- the demo --------------------------------------------------------------

console.log(`logs in ${dir}`);
const server = spawn(SERVER, ['--base-dir', join(dir, 'server')], {
  env: {
    ...process.env,
    RESTATE_BIND_IP: '127.0.0.1',
    RESTATE_LISTEN_MODE: 'tcp',
    // Scopes and limits, on a 1.7 server (on by default from 1.8).
    RESTATE_EXPERIMENTAL_ENABLE_VQUEUES: 'true',
    RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V7: 'true',
  },
  stdio: ['ignore', logFile('server'), logFile('server')],
});
children.add(server);
const upstream = startUpstream(UPSTREAM_PORT);
await upstream.ready;
const stats = upstream.stats;
const mcp = createServer((req, res) => void mcpHandler(ingressGateway(INGRESS))(req, res));
await new Promise<void>((r) => mcp.listen(MCP_PORT, '127.0.0.1', r));
mcp.unref();
upstream.server.unref();

const env = { UPSTREAM_URL: `http://127.0.0.1:${UPSTREAM_PORT}`, RESTATE_ADMIN_URL: ADMIN };
supervise('tools', 'tools.ts', 9411, env);
supervise('gateway', 'gateway.ts', 9412, env);
const agents = supervise('agents', 'agent.ts', 9413, { ...env, ABORT_PER_MILLE: String(AGENT_PER_MILLE) });
await waitFor(9070);
await waitFor(8080);
for (const port of [9411, 9412, 9413]) {
  await waitFor(port);
  await admin('POST', '/deployments', { uri: `http://127.0.0.1:${port}` });
}
await limitRules({ disabled: true });

act(
  'The cast',
  [
    '  Tools     Orders.lookup, Payments.refund, Mail.emailCustomer: plain Restate services, with the journal',
    '  Gateway   finds them by their metadata in the admin API; checks policy, holds for approval,',
    "            calls each in the tenant's scope with `<tool>/<user>` as limit key, and audits it",
    '  Tenant    a virtual object per tenant whose state is SQL: the audit log and the approvals',
    "  Assistant a pi-durable agent per user, its session in its object's SQLite; it crashes at random",
    `  Upstream  a fake order system that serves ${CAPACITY} requests at once per tenant, and 429s the rest`,
  ].join('\n'),
);
const tools = (await ingress<{ name: string }[]>('/Gateway/tools', { tenant: 'acme' })).map((t) => t.name);
say(`acme's tools: ${tools.join(', ')}`);
assert.deepEqual(tools.toSorted(), ['email_customer', 'lookup_order', 'refund']);

// 1. Without a limit.
act(
  '1. A burst, without a limit',
  '  alice (acme) asks about 12 orders; the model calls lookup_order 12 times at once.',
);
const before = { ...stats.orders.acme, refused: stats.orders.acme?.refused ?? 0 };
const unlimited = await timed(watched(prompt('acme.alice', 'p1', 'where are orders 101 to 112?')));
const refusedUnlimited = stats.orders.acme!.refused - before.refused;
say(
  `answered in ${unlimited.ms} ms; the order system refused ${refusedUnlimited} requests with a 429, which the tools retried`,
);
assert.equal(unlimited.value.split('\n').length, 12);

// 2. With a limit.
act(
  '2. The same burst, with a limit rule',
  `  restate rules set '*/lookup_order' --concurrency ${CAPACITY}   (here: PUT /limits/rules, no redeploy)\n` +
    '  alice asks about 16 more orders, and dan (globex) about 3, at the same time.',
);
await limitRules({});
const refusedBefore = { acme: stats.orders.acme!.refused, globex: stats.orders.globex?.refused ?? 0 };
for (const t of Object.values(stats.orders)) Object.assign(t, { peak: 0, first: 0, last: 0 });
const [acme, globex] = await watched(
  Promise.all([
    timed(prompt('acme.alice', 'p2', 'where are orders 113 to 128?')),
    sleep(300).then(() => timed(prompt('globex.dan', 'p3', 'where are orders 201 to 203?'))),
  ]),
);
say(`alice: 16 orders in ${acme.ms} ms; dan: 3 orders in ${globex.ms} ms`);
say(
  `the order system: at most ${stats.orders.acme!.peak} of acme's requests at once, ${stats.orders.globex!.peak} of globex's; ` +
    `refused ${stats.orders.acme!.refused - refusedBefore.acme + stats.orders.globex!.refused - refusedBefore.globex}`,
);
assert.ok(stats.orders.acme!.peak <= CAPACITY && stats.orders.globex!.peak <= CAPACITY);
assert.equal(stats.orders.acme!.refused, refusedBefore.acme);
assert.equal(stats.orders.globex!.refused, refusedBefore.globex);
// globex's lookups do not wait in acme's queue: the first reached the order
// system while acme's still went in.
assert.ok(stats.orders.globex!.first < stats.orders.acme!.last, "globex's calls do not wait for acme's");
say("globex's first lookup reached the order system while acme's queue still drained");

// 3. Users within a tenant.
/** Forget the peaks and times of the order system's requests so far. */
const resetPeaks = () => {
  for (const t of [...Object.values(stats.orders), ...Object.values(stats.users)]) {
    Object.assign(t, { peak: 0, first: 0, last: 0 });
  }
};
/** Wait until at least `n` of `user`'s lookups at acme are queued. */
const queued = async (user: string, n: number) => {
  for (const deadline = Date.now() + 60_000; Date.now() < deadline;) {
    const rows = await query<{ num_inbox: number }>(
      `SELECT num_inbox FROM sys_vqueue_meta WHERE scope = 'acme' AND limit_key = 'lookup_order/${user}'`,
    ).catch(() => []);
    if (rows.some((r) => r.num_inbox >= n)) return;
    await sleep(50);
  }
  throw new Error(`${user}'s lookups never queued`);
};
// erin, of acme, from an MCP client (Claude Code, say): her lookups at once.
const erin = mcpClient('acme.erin');
await erin.send('initialize', {
  protocolVersion: '2025-06-18',
  capabilities: {},
  clientInfo: { name: 'demo', version: '1' },
});
const erinLooksUp = (orders: string[]) => Promise.all(orders.map((order) => erin.call('lookup_order', { order })));
act(
  '3. Users within a tenant',
  "  The limit key is `lookup_order/<user>`: each user's calls queue apart, and the queues take turns.\n" +
    '  alice asks about 16 orders; while they queue, erin (acme too) looks up 2 from an MCP client.',
);
resetPeaks();
const queues: Queue[] = [];
await watched(
  Promise.all([
    prompt('acme.alice', 'p3a', 'where are orders 131 to 146?'),
    queued('alice', 8).then(() => erinLooksUp(['151', '152'])),
  ]),
  queues,
);
// When erin's first lookup ran, how many of alice's were still queued.
const erinRan = queues.find((q) => q.user === 'erin' && q.running > 0);
const aliceThen = queues.find((q) => q.user === 'alice' && erinRan && q.at === erinRan.at);
if (erinRan) say(`erin's lookups ran while ${aliceThen?.queued ?? 0} of alice's still waited`);
assert.ok(stats.orders.acme!.peak <= CAPACITY);
assert.ok(stats.users['acme/erin']!.first < stats.users['acme/alice']!.last, "erin's calls do not wait for alice's");
say("erin's first lookup reached the order system while alice's queue still drained");

act(
  '   and a limit per user',
  "  restate rules set '*/lookup_order/*' --concurrency 1   (the second level: each user)\n" +
    '  alice asks about 6 orders, and erin looks up 2: each runs one at a time, and neither waits for the other.',
);
await limitRules({ perUser: 1 });
resetPeaks();
await watched(
  Promise.all([
    prompt('acme.alice', 'p3c', 'where are orders 161 to 166?'),
    queued('alice', 3).then(() => erinLooksUp(['171', '172'])),
  ]),
);
say(
  `the order system: at most ${stats.users['acme/alice']!.peak} of alice's requests at once, ` +
    `${stats.users['acme/erin']!.peak} of erin's, ${stats.orders.acme!.peak} of acme's`,
);
assert.equal(stats.users['acme/alice']!.peak, 1);
assert.equal(stats.users['acme/erin']!.peak, 1);
assert.ok(stats.users['acme/erin']!.first < stats.users['acme/alice']!.last);

// 4. Approval.
act('4. Human in the loop', '  bob (acme) asks for a refund; a manager must approve it.');
const refund = prompt('acme.bob', 'p4', 'refund 30 of order 103 because it arrived broken');
let pending: { awakeable: string; tool: string; arguments: unknown }[] = [];
while (pending.length === 0) {
  await sleep(200);
  pending = await ingress('/Tenant/acme/pending', {});
}
say(`held: ${pending[0]!.tool} ${JSON.stringify(pending[0]!.arguments)}`);
await sleep(1500);
const suspended = await query<{ target: string; status: string }>(
  "SELECT target, status FROM sys_invocation WHERE target_service_name IN ('Assistant', 'Gateway') AND status != 'completed'",
);
say(`meanwhile: ${suspended.map((s) => `${s.target} ${s.status}`).join(', ')}`);
say("kill -9 the agents' process while the refund waits");
agents.child?.kill('SIGKILL');
await sleep(1000);
say(
  'the manager approves: curl localhost:8080/Tenant/acme/decide --json \'{"awakeable": "…", "approved": true, "by": "carol"}\'',
);
await ingress('/Tenant/acme/decide', { awakeable: pending[0]!.awakeable, approved: true, by: 'carol' });
say(`bob's answer: ${await refund}`);
say(`the payment provider made ${stats.refunds.made.length} refund, of ${stats.refunds.requests} request(s)`);
assert.equal(stats.refunds.made.length, 1);

// 4. MCP.
act(
  '5. MCP',
  "  An MCP client with dan's token at globex (`globex.dan`), as Claude Code would be: tools/list, tools/call.",
);
const client = mcpClient('globex.dan');
await client.send('initialize', {
  protocolVersion: '2025-06-18',
  capabilities: {},
  clientInfo: { name: 'demo', version: '1' },
});
await client.send('notifications/initialized', undefined, null);
const listed = (await client.send('tools/list')) as { tools: { name: string }[] };
say(`tools/list: ${listed.tools.map((t) => t.name).join(', ')}`);
const looked = await client.call('lookup_order', { order: '204' });
say(`lookup_order 204: ${looked.content[0]!.text}`);
const refused = await client.call('refund', { order: '204', amount: 5, reason: 'asking anyway' });
say(`refund: ${refused.isError ? 'refused' : 'made'}: ${refused.content[0]!.text}`);
assert.equal(refused.isError, true);
const mailsBefore = stats.mails.sent;
const mailed = await Promise.all([
  client.call('email_customer', { order: '204', text: 'Your order is on its way.' }, 99),
  client.call('email_customer', { order: '204', text: 'Your order is on its way.' }, 99),
]);
say(`email_customer, sent twice with one JSON-RPC id: ${mailed.map((m) => m.content[0]!.text).join(' / ')}`);
say(`the mail provider sent ${stats.mails.sent - mailsBefore} mail`);
assert.equal(stats.mails.sent - mailsBefore, 1);

// 5. The audit log.
act('6. The audit log', "  Tenant/acme/report: a query of the tenant's SQL table of calls.");
await sleep(500); // the gateway's last records are sends
const report = await ingress<{ tools: Record<string, unknown>[]; users: Record<string, unknown>[] }>(
  '/Tenant/acme/report',
  {},
);
console.table(report.tools);
console.table(report.users);
const globexReport = await ingress<{ tools: { tool: string; calls: number; refused: number }[] }>(
  '/Tenant/globex/report',
  {},
);
console.table(globexReport.tools);
const lookups = report.tools.find((t) => t.tool === 'lookup_order');
assert.equal(lookups?.calls, 12 + 16 + 18 + 8, 'one record per lookup, however often the agents crashed');
assert.equal(globexReport.tools.find((t) => t.tool === 'refund')?.refused, 1);

const invocations = await query<{ target: string; n: number }>(
  "SELECT target, count(*) AS n FROM sys_invocation WHERE target_service_name IN ('Orders', 'Payments', 'Mail') GROUP BY target ORDER BY target",
);
say(`tool invocations: ${invocations.map((i) => `${i.target} ${i.n}`).join(', ')}`);
assert.deepEqual(
  Object.fromEntries(invocations.map((i) => [i.target, i.n])),
  { 'Mail/emailCustomer': 1, 'Orders/lookup': 12 + 16 + 18 + 8 + 4, 'Payments/refund': 1 },
  'one tool invocation per tool call',
);
console.log(`\n${bold('passed')}: the agents' process restarted ${agents.restarts} times.`);
stopAll();
process.exit(0);

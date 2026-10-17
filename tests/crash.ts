// The examples against a real Restate server, through crashes: the services
// abort at random state requests, and the server is killed twice, while
// clients place, pay and expire orders and prompt agents. The ledger and the
// shop run in one process (ABORT_PER_MILLE, default 15), and each kind of
// agent in its own: the pi, LangGraph and DeepSeek Harness agents
// (AGENT_ABORT_PER_MILLE, default 3: a pi or DeepSeek Harness prompt makes
// some hundreds of state requests), the Claude agents (ABORT_PER_MILLE), and
// the OpenCode agents (OPENCODE_ABORT_PER_MILLE, default 1: an OpenCode prompt
// makes some hundreds of state requests too, and its process takes seconds to
// start).
// The agents get the same prompts. Each request is resent until answered,
// some twice.
//
// The agents' models are the scripted tellers (fakes/fakes.ts), which need
// no API key: "move 5 from 1 to 2" makes a transfer.
//
// Checks: the warehouses' books balance and agree with every order, and every
// agent prompt moved money exactly once.
//
//   RESTATE_SERVER=/path/to/restate-server node test.ts   (or make crash-test)
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync, rmSync } from 'node:fs';
import { connect } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import type { ChildProcess } from 'node:child_process';
import type { Audit } from './sql/ledger.ts';
import type { Order, Reservation } from './sql/shop.ts';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PER_MILLE = Number(process.env.ABORT_PER_MILLE ?? 15);
const AGENT_PER_MILLE = Number(process.env.AGENT_ABORT_PER_MILLE ?? 3);
const OPENCODE_PER_MILLE = Number(process.env.OPENCODE_ABORT_PER_MILLE ?? 1);
/** A service process: its name, the groups of serve.ts it serves, its port and abort rate. */
interface Process {
  name: string;
  groups: string[];
  port: number;
  perMille: number;
}
const PROCESSES: Process[] = [
  { name: 'shop', groups: ['ledger', 'shop'], port: 9182, perMille: PER_MILLE },
  { name: 'agent', groups: ['agent'], port: 9183, perMille: AGENT_PER_MILLE },
  { name: 'langgraph', groups: ['langgraph'], port: 9186, perMille: AGENT_PER_MILLE },
  // A Claude prompt makes few state requests: it aborts as often as the shop.
  { name: 'claude', groups: ['claude'], port: 9187, perMille: PER_MILLE },
  { name: 'deepseek', groups: ['deepseek'], port: 9188, perMille: AGENT_PER_MILLE },
  { name: 'opencode', groups: ['opencode'], port: 9189, perMille: OPENCODE_PER_MILLE },
];
// The agents: the pi one, the LangGraph one, the Claude one, the DeepSeek
// Harness one and the OpenCode one, with the same prompts.
const AGENTS = ['Agent', 'GraphAgent', 'ClaudeAgent', 'DeepSeekAgent', 'OpenCodeAgent'];
const FAKES = fileURLToPath(new URL('fakes/fakes.ts', import.meta.url));
const EXPIRY_MS = 1500;
const SKUS = ['apple', 'pear', 'plum'];
const STOCK = 20;
const ORDERS = 60;
const SESSIONS = 4;
const PROMPTS = 8;
const ACCOUNTS = 8;
const OPENING = 1000;

const dir = mkdtempSync(join(tmpdir(), 'sqlite-kv-examples-'));
// One descriptor per log, however often its process restarts.
const logs = new Map<string, number>();
const log = (name: string) => logs.get(name) ?? logs.set(name, openSync(join(dir, name), 'a')).get(name)!;
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
/** Wait until `done()`, which other tasks make true. */
const until = async (done: () => boolean) => {
  while (!done()) await sleep(20);
};
console.error(`logs in ${dir}`);

const startServer = () =>
  spawn(SERVER, ['--base-dir', join(dir, 'server')], {
    env: { ...process.env, RESTATE_BIND_IP: '127.0.0.1', RESTATE_LISTEN_MODE: 'tcp' },
    stdio: ['ignore', log('server.log'), log('server.log')],
  });

async function healthy() {
  for (const deadline = Date.now() + 60_000; Date.now() < deadline; await sleep(200)) {
    if (
      await fetch(`${ADMIN}/health`).then(
        (r) => r.ok,
        () => false,
      )
    )
      return;
  }
  throw new Error('restate-server did not start');
}

/** A supervised process: how many times it started, and its current child. */
interface Supervised {
  starts: number;
  stopped: boolean;
  child: ChildProcess;
  stop(): Promise<void>;
}

/** Whether something listens on `port`. */
const listening = (port: number) =>
  new Promise<boolean>((resolve) => {
    const socket = connect(port, '127.0.0.1');
    socket.once('connect', () => (socket.destroy(), resolve(true)));
    socket.once('error', () => resolve(false));
  });

/**
 * Keep `process` running, restarting it whenever it aborts. An abort (of
 * faults.ts) is a SIGKILL; any other exit is a failure, such as a port taken
 * at startup, restarted after a pause, and ending the run if it repeats.
 */
function supervise({ name, groups, port }: Process, perMille: number): Supervised {
  let failures = 0;
  const run = () =>
    spawn(process.execPath, ['--import', FAKES, fileURLToPath(new URL('serve.ts', import.meta.url)), ...groups], {
      env: {
        ...process.env,
        PI_MODEL: 'teller/teller',
        OPENCODE_MODEL: 'teller/teller',
        PORT: String(port),
        ABORT_PER_MILLE: String(perMille),
        SHOP_EXPIRY_MS: String(EXPIRY_MS),
        SQLITE_KV_CACHE_FILE: join(dir, `${name}.cache`),
      },
      stdio: ['ignore', 'ignore', log(`${name}.log`)],
    });
  const restart = () => {
    if (state.stopped) return;
    state.starts++;
    state.child = run();
    state.child.on('exit', exited);
  };
  const exited = (code: number | null, signal: NodeJS.Signals | null) => {
    if (state.stopped) return;
    if (signal === 'SIGKILL') {
      failures = 0;
      return restart();
    }
    if (++failures >= 5) {
      console.error(`${name} exited (${signal ?? code}) ${failures} times in a row; see ${join(dir, `${name}.log`)}`);
      process.exit(1);
    }
    setTimeout(restart, 500);
  };
  const state: Supervised = {
    starts: 1,
    stopped: false,
    child: run(),
    stop: () =>
      new Promise((resolve) => {
        state.stopped = true;
        state.child.removeAllListeners('exit');
        state.child.on('exit', () => resolve());
        state.child.kill();
      }),
  };
  state.child.on('exit', exited);
  return state;
}

/** POST until answered with 200, through crashes of either side. */
async function call<T = unknown>(path: string, body?: unknown, headers: Record<string, string> = {}): Promise<T> {
  const deadline = Date.now() + 300_000;
  for (;;) {
    let response;
    try {
      response = await fetch(`${INGRESS}/${path}`, {
        method: 'POST',
        headers: body === undefined ? headers : { ...headers, 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(30_000),
      });
    } catch {
      response = null;
    }
    if (response?.ok) {
      const text = await response.text();
      return (text ? JSON.parse(text) : undefined) as T;
    }
    if (response && response.status !== 404) {
      const text = await response.text();
      // A failed invocation, unlike an unavailable server, is final.
      if (response.status < 500 || text.includes('"source":"invocation"')) {
        throw new Error(`${path}: ${response.status} ${text}`);
      }
    }
    if (Date.now() > deadline) throw new Error(`${path}: no answer`);
    await sleep(100);
  }
}

/** Run `tasks` (functions) with at most `width` at a time. */
async function pool(width: number, tasks: (() => Promise<void>)[]) {
  const queue = [...tasks];
  await Promise.all(
    Array.from({ length: width }, async () => {
      for (let task = queue.shift(); task; task = queue.shift()) await task();
    }),
  );
}

// A server left running (by a run that was killed) would serve this run its
// state, and services left running would take the ports and run old code:
// refuse to start against them.
if (
  await fetch(`${ADMIN}/health`).then(
    () => true,
    () => false,
  )
) {
  throw new Error(`a Restate server already listens at ${ADMIN}; stop it first`);
}
for (const { name, port } of PROCESSES) {
  if (await listening(port))
    throw new Error(`port ${port} (${name}) is taken (by a run that was killed?): stop it first`);
}
let server = startServer();
let services = PROCESSES.map((p) => supervise(p, p.perMille));
// On failure, or when interrupted or terminated, leave nothing running.
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP'] as const) process.on(signal, () => process.exit(1));
process.on('exit', () => {
  for (const service of services) {
    service.stopped = true;
    service.child.kill('SIGKILL');
  }
  server.kill('SIGKILL');
});
await healthy();
for (const { port } of PROCESSES) {
  for (const deadline = Date.now() + 60_000; ; await sleep(200)) {
    const registered = await fetch(`${ADMIN}/deployments`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ uri: `http://127.0.0.1:${port}`, force: true }),
    }).then(
      (r) => r.ok,
      () => false,
    );
    if (registered) break;
    if (Date.now() > deadline) throw new Error('registration failed');
  }
}

await call('Ledger/bank/init', [ACCOUNTS, OPENING]);
for (const sku of SKUS) {
  await call(`Warehouse/${sku}/restock`, { id: 'first', qty: STOCK });
  await call(`Warehouse/${sku}/restock`, { id: 'first', qty: STOCK }); // a resend
}

let progress = 0;
const total = ORDERS * 2 + AGENTS.length * SESSIONS * (PROMPTS + 1);
const step = () => {
  if (++progress % 25 === 0) console.error(`${progress}/${total} requests answered`);
};

// Orders: place each (some twice), then pay the even ones (which may have
// expired first).
const paid = new Map<string, string>();
const order = (i: number) => ({ id: `o${i}`, sku: SKUS[i % SKUS.length]!, qty: 1 + (i % 3) });
const shop = pool(
  6,
  Array.from({ length: ORDERS }, (_, i) => async () => {
    const { id, sku, qty } = order(i);
    for (let n = 0; n < (i % 4 === 0 ? 2 : 1); n++) await call(`Orders/${id}/place`, { sku, qty });
    step();
    if (i % 2 === 0) paid.set(id, await call<string>(`Orders/${id}/pay`));
    step();
  }),
);

// Agents: transfers, then an audit, per session of each agent.
const sessions = AGENTS.flatMap((name) => Array.from({ length: SESSIONS }, (_, s) => [name, s] as const));
const agents = pool(
  sessions.length,
  sessions.map(([name, s]) => async () => {
    for (let p = 0; p < PROMPTS; p++) {
      const from = (s + p) % ACCOUNTS;
      const to = (s + 3 * p + 1) % ACCOUNTS;
      const id = `s${s}-p${p}`;
      const text = `Please move ${1 + p} from ${from} to ${to === from ? (to + 1) % ACCOUNTS : to}`;
      // Sent again, by its idempotency key: the same invocation, and answer.
      for (let n = 0; n < (p % 3 === 0 ? 2 : 1); n++) {
        const answer = await call<string>(`${name}/s${s}/chat`, text, { 'idempotency-key': id });
        assert.match(answer, /^Done: moved/, `${name} ${id}: ${answer}`);
      }
      step();
    }
    const answer = await call<string>(`${name}/s${s}/chat`, 'audit the books', { 'idempotency-key': `s${s}-audit` });
    assert.match(answer, /"inconsistent":0/, answer);
    step();
  }),
);

// Kill the server twice along the way.
const kills = (async () => {
  for (const at of [total / 3, (2 * total) / 3]) {
    await until(() => progress >= at);
    console.error(`killing the server after ${progress} requests`);
    const exited = new Promise((r) => server.on('exit', r));
    server.kill('SIGKILL');
    await exited;
    server = startServer();
    await healthy();
  }
})();
await Promise.all([shop, agents, kills]);
const starts = services.map((s) => s.starts);
await Promise.all(services.map((s) => s.stop()));
services = PROCESSES.map((p) => supervise(p, 0));

// Unpaid orders expire.
const statuses = new Map<string, Order>();
for (const deadline = Date.now() + 120_000; ; await sleep(500)) {
  for (let i = 0; i < ORDERS; i++) statuses.set(`o${i}`, await call<Order>(`Orders/o${i}/status`));
  if (![...statuses.values()].some((o) => o.status === 'reserved')) break;
  if (Date.now() > deadline) throw new Error('orders did not expire');
}
const expected: Record<string, string> = { paid: 'sold', expired: 'released', rejected: 'rejected' };
const counts: Record<string, number> = {};
for (const sku of SKUS) {
  const { available, restocked, reservations } = await call<{
    available: number;
    restocked: number;
    reservations: Reservation[];
  }>(`Warehouse/${sku}/report`);
  assert.equal(restocked, STOCK, sku);
  const sold = reservations.filter(([, , state]) => state === 'sold').reduce((sum, [, qty]) => sum + qty, 0);
  assert.equal(available + sold, restocked, `${sku}: available ${available}, sold ${sold}`);
  const mine = Array.from({ length: ORDERS }, (_, i) => order(i)).filter((o) => o.sku === sku);
  assert.equal(reservations.length, mine.length, `${sku}: one reservation per order`);
  for (const [id, qty, state] of reservations) {
    const o = statuses.get(id)!;
    assert.equal(expected[o.status], state, `${id}: ${o.status} but ${state}`);
    assert.equal(o.qty, qty, id);
    counts[o.status] = (counts[o.status] ?? 0) + 1;
  }
}
for (const [id, answer] of paid) {
  if (answer === 'paid') assert.equal(statuses.get(id)?.status, 'paid', `${id} was paid`);
}

const audit = await call<Audit>('Ledger/bank/audit');
assert.deepEqual(audit, {
  accounts: ACCOUNTS,
  total: ACCOUNTS * OPENING,
  transfers: AGENTS.length * SESSIONS * PROMPTS, // exactly one per prompt
  inconsistent: 0,
});

// Retried attempts reached the invocations their first attempt started: one
// reserve and one ledger transfer per order and prompt, though attempts of
// Orders/place and Agent/chat ran many times.
const invocations = async (service: string, handler: string): Promise<number> => {
  const response = await fetch(`${ADMIN}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({
      query: `SELECT count(*) AS n FROM sys_invocation WHERE target_service_name = '${service}' AND target_handler_name = '${handler}'`,
    }),
  });
  const { rows } = (await response.json()) as { rows: { n: number }[] };
  return rows[0]!.n;
};
assert.equal(await invocations('Warehouse', 'reserve'), ORDERS, 'reserve invocations');
assert.equal(await invocations('Ledger', 'transfer'), AGENTS.length * SESSIONS * PROMPTS, 'transfer invocations');

console.error(`orders: ${JSON.stringify(counts)}`);
console.error(`ledger: ${JSON.stringify(audit)}`);
PROCESSES.forEach((p, i) => console.error(`${p.name} process started ${starts[i]} times`));
PROCESSES.forEach((p, i) => assert.ok(p.perMille === 0 || (starts[i] ?? 0) > 1, `no abort happened in ${p.name}`));

await Promise.all(services.map((s) => s.stop()));
server.kill('SIGKILL');
rmSync(dir, { recursive: true, force: true });
console.error('ok');

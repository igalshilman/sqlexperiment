// The examples against a real Restate server, through crashes: the services
// abort at random state requests, and the server is killed twice, while
// clients place, pay and expire orders and prompt agents. The ledger and the
// shop run in one process (ABORT_PER_MILLE, default 15), the agents in another
// (AGENT_ABORT_PER_MILLE, default 3: a prompt makes some 200 state requests).
// Each request is resent until answered, some twice.
//
// Checks: the warehouses' books balance and agree with every order, and every
// agent prompt moved money exactly once.
//
//   RESTATE_SERVER=/path/to/restate-server node test.mjs   (or make crash-test-node)
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PER_MILLE = Number(process.env.ABORT_PER_MILLE ?? 15);
const AGENT_PER_MILLE = Number(process.env.AGENT_ABORT_PER_MILLE ?? 3);
const PROCESSES = [
  { name: 'shop', groups: ['ledger', 'shop'], port: 9182, perMille: PER_MILLE },
  { name: 'agent', groups: ['agent'], port: 9183, perMille: AGENT_PER_MILLE },
];
const EXPIRY_MS = 1500;
const SKUS = ['apple', 'pear', 'plum'];
const STOCK = 20;
const ORDERS = 60;
const SESSIONS = 4;
const PROMPTS = 8;
const ACCOUNTS = 8;
const OPENING = 1000;

const dir = mkdtempSync(join(tmpdir(), 'sqlite-kv-examples-'));
const log = (name) => openSync(join(dir, name), 'a');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
console.error(`logs in ${dir}`);

const startServer = () =>
  spawn(SERVER, ['--base-dir', join(dir, 'server')], {
    env: { ...process.env, RESTATE_BIND_IP: '127.0.0.1', RESTATE_LISTEN_MODE: 'tcp' },
    stdio: ['ignore', log('server.log'), log('server.log')],
  });

async function healthy() {
  for (const deadline = Date.now() + 60_000; Date.now() < deadline; await sleep(200)) {
    if (await fetch(`${ADMIN}/health`).then((r) => r.ok, () => false)) return;
  }
  throw new Error('restate-server did not start');
}

/** Keep `process` running, restarting it whenever it aborts. */
function supervise({ name, groups, port }, perMille) {
  const state = { starts: 0, stopped: false, child: null };
  const start = () => {
    if (state.stopped) return;
    state.starts++;
    state.child = spawn(process.execPath, [fileURLToPath(new URL('serve.mjs', import.meta.url)), ...groups], {
      env: {
        ...process.env,
        PORT: String(port),
        ABORT_PER_MILLE: String(perMille),
        SHOP_EXPIRY_MS: String(EXPIRY_MS),
        SQLITE_KV_CACHE_FILE: join(dir, `${name}.cache`),
      },
      stdio: ['ignore', 'ignore', log(`${name}.log`)],
    });
    state.child.on('exit', start);
  };
  start();
  state.stop = () =>
    new Promise((resolve) => {
      state.stopped = true;
      state.child.removeAllListeners('exit');
      state.child.on('exit', resolve);
      state.child.kill();
    });
  return state;
}

/** POST until answered with 200, through crashes of either side. */
async function call(path, body) {
  const deadline = Date.now() + 300_000;
  for (;;) {
    let response;
    try {
      response = await fetch(`${INGRESS}/${path}`, {
        method: 'POST',
        headers: body === undefined ? {} : { 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(30_000),
      });
    } catch {
      response = null;
    }
    if (response?.ok) {
      const text = await response.text();
      return text ? JSON.parse(text) : undefined;
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
async function pool(width, tasks) {
  const queue = [...tasks];
  await Promise.all(Array.from({ length: width }, async () => {
    while (queue.length > 0) await queue.shift()();
  }));
}

let server = startServer();
let services = PROCESSES.map((p) => supervise(p, p.perMille));
// On failure too, leave nothing running.
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
    }).then((r) => r.ok, () => false);
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
const total = ORDERS * 2 + SESSIONS * (PROMPTS + 1);
const step = () => {
  if (++progress % 25 === 0) console.error(`${progress}/${total} requests answered`);
};

// Orders: place each (some twice), then pay the even ones (which may have
// expired first).
const paid = new Map();
const order = (i) => ({ id: `o${i}`, sku: SKUS[i % SKUS.length], qty: 1 + (i % 3) });
const shop = pool(6, Array.from({ length: ORDERS }, (_, i) => async () => {
  const { id, sku, qty } = order(i);
  for (let n = 0; n < (i % 4 === 0 ? 2 : 1); n++) await call(`Orders/${id}/place`, { sku, qty });
  step();
  if (i % 2 === 0) paid.set(id, await call(`Orders/${id}/pay`));
  step();
}));

// Agents: transfers, then an audit, per session.
const prompts = [];
const agents = pool(SESSIONS, Array.from({ length: SESSIONS }, (_, s) => async () => {
  for (let p = 0; p < PROMPTS; p++) {
    const from = (s + p) % ACCOUNTS;
    const to = (s + 3 * p + 1) % ACCOUNTS;
    const prompt = { id: `s${s}-p${p}`, text: `Please move ${1 + p} from ${from} to ${to === from ? (to + 1) % ACCOUNTS : to}` };
    prompts.push(prompt);
    for (let n = 0; n < (p % 3 === 0 ? 2 : 1); n++) {
      const { answer } = await call(`Agent/s${s}/prompt`, prompt);
      assert.match(answer, /^Done: moved/, `${prompt.id}: ${answer}`);
    }
    step();
  }
  const { answer } = await call(`Agent/s${s}/prompt`, { id: `s${s}-audit`, text: 'audit the books' });
  assert.match(answer, /"inconsistent":0/, answer);
  step();
}));

// Kill the server twice along the way.
const kills = (async () => {
  for (const at of [total / 3, (2 * total) / 3]) {
    while (progress < at) await sleep(20);
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
const statuses = new Map();
for (const deadline = Date.now() + 120_000; ; await sleep(500)) {
  for (let i = 0; i < ORDERS; i++) statuses.set(`o${i}`, await call(`Orders/o${i}/status`));
  if (![...statuses.values()].some((o) => o.status === 'reserved')) break;
  if (Date.now() > deadline) throw new Error('orders did not expire');
}
const expected = { paid: 'sold', expired: 'released', rejected: 'rejected' };
const counts = {};
for (const sku of SKUS) {
  const { available, restocked, reservations } = await call(`Warehouse/${sku}/report`);
  assert.equal(restocked, STOCK, sku);
  const sold = reservations.filter(([, , state]) => state === 'sold').reduce((sum, [, qty]) => sum + qty, 0);
  assert.equal(available + sold, restocked, `${sku}: available ${available}, sold ${sold}`);
  const mine = Array.from({ length: ORDERS }, (_, i) => order(i)).filter((o) => o.sku === sku);
  assert.equal(reservations.length, mine.length, `${sku}: one reservation per order`);
  for (const [id, qty, state] of reservations) {
    const o = statuses.get(id);
    assert.equal(expected[o.status], state, `${id}: ${o.status} but ${state}`);
    assert.equal(o.qty, qty, id);
    counts[o.status] = (counts[o.status] ?? 0) + 1;
  }
}
for (const [id, answer] of paid) {
  if (answer === 'paid') assert.equal(statuses.get(id).status, 'paid', `${id} was paid`);
}

const audit = await call('Ledger/bank/audit');
assert.deepEqual(audit, {
  accounts: ACCOUNTS,
  total: ACCOUNTS * OPENING,
  transfers: SESSIONS * PROMPTS, // exactly one per prompt
  inconsistent: 0,
});

// Retried attempts reached the invocations their first attempt started: one
// reserve and one ledger transfer per order and prompt, though attempts of
// Orders/place and Agent/prompt ran many times.
const invocations = async (service, handler) => {
  const response = await fetch(`${ADMIN}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({
      query: `SELECT count(*) AS n FROM sys_invocation WHERE target_service_name = '${service}' AND target_handler_name = '${handler}'`,
    }),
  });
  const { rows } = await response.json();
  return rows[0].n;
};
assert.equal(await invocations('Warehouse', 'reserve'), ORDERS, 'reserve invocations');
assert.equal(await invocations('Ledger', 'transfer'), SESSIONS * PROMPTS, 'transfer invocations');

console.error(`orders: ${JSON.stringify(counts)}`);
console.error(`ledger: ${JSON.stringify(audit)}`);
PROCESSES.forEach((p, i) => console.error(`${p.name} process started ${starts[i]} times`));
PROCESSES.forEach((p, i) => assert.ok(p.perMille === 0 || starts[i] > 1, `no abort happened in ${p.name}`));

await Promise.all(services.map((s) => s.stop()));
server.kill('SIGKILL');
rmSync(dir, { recursive: true, force: true });
console.error('ok');

// SQL in virtual objects against a real Restate server: throughput, latency
// and what the objects store, for a few workloads and for databases of 1, 10
// and 100 MB. A benchmark, not a test: it measures the storage protocol end
// to end, with the SDK and the server. Rows hold what an agent stores: model
// replies, tool calls and tool output, as generated text that compresses
// about as such text does; with PAYLOAD=random, random bytes, which do not.
//
//   RESTATE_SERVER=/path/to/restate-server node sql/bench.ts [out.json]
//
// It starts the server in a temporary directory and the objects in a child
// process (so that a phase can restart them, cold), and prints one JSON row
// per phase. Stored bytes and keys are read from the server's state table.
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync, rmSync, writeFileSync } from 'node:fs';
import { connect } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import * as restate from 'restate-fns';
import { row } from './sql.ts';

const SCHEMA = `CREATE TABLE IF NOT EXISTS rows(id INTEGER PRIMARY KEY, v);
  CREATE TABLE IF NOT EXISTS counter(id INTEGER PRIMARY KEY, n INTEGER);
  CREATE TABLE IF NOT EXISTS history(at INTEGER PRIMARY KEY, n INTEGER)`;

// Words of model replies and tool output, for generated text.
const WORDS = `the a to of and in is for that this with it on be as are can file function
  error test run value return code line we will I let me check update change add use need
  first then now next should would could here which when if not by from or an at so all
  build output result first step config path type string number list object request response
  call tool user assistant message model context state database query table row column key
  handler service invocation retry commit page log cache read write open close start end
  failed passed warning info debug src lib index main module import export const async await`.split(/\s+/);
const TOOLS = ['read_file', 'edit_file', 'run_command', 'search', 'list_dir', 'fetch_url'];
const pick = <T>(xs: T[]) => xs[Math.floor(Math.random() * xs.length)]!;
const sentence = () => {
  const words = Array.from({ length: 6 + Math.floor(Math.random() * 14) }, () => pick(WORDS));
  return `${words.join(' ')}.`;
};
const hexId = () => randomBytes(8).toString('hex');

/** One message of an agent's transcript, of about `bytes` bytes, as JSON. */
function agentMessage(bytes: number): string {
  const text = () => {
    let out = '';
    while (out.length < bytes) out += `${sentence()} `;
    return out.slice(0, bytes);
  };
  const lines = () => {
    let out = '';
    for (let n = 1; out.length < bytes; n++) {
      out += `${pick(['src', 'lib', 'tests'])}/${pick(WORDS)}_${pick(WORDS)}.ts:${n * 7}: ${pick(['error', 'warning', 'info'])} ${sentence()}\n`;
    }
    return out.slice(0, bytes);
  };
  switch (Math.floor(Math.random() * 3)) {
    case 0:
      return JSON.stringify({ role: 'assistant', id: hexId(), content: text() });
    case 1: {
      const args = { path: `src/${pick(WORDS)}/${pick(WORDS)}.ts`, query: sentence(), body: text() };
      return JSON.stringify({
        role: 'assistant',
        id: hexId(),
        tool_call: { id: hexId(), name: pick(TOOLS), arguments: args },
      });
    }
    default:
      return JSON.stringify({ role: 'tool', tool_call_id: hexId(), name: pick(TOOLS), output: lines() });
  }
}

/** A row's value of about `bytes` bytes: a message, or random bytes with PAYLOAD=random. */
const payload = (bytes: number) => (process.env.PAYLOAD === 'random' ? randomBytes(bytes) : agentMessage(bytes));

/** The benchmark's object: each key a database. */
export const bench = restate.object({
  name: 'SqlBench',
  handlers: {
    /** Insert one row of `bytes` bytes, an autocommit statement. */
    insert: async (ctx: restate.ObjectContext, bytes: number) => {
      await ctx.db.execute(SCHEMA);
      await ctx.db.execute('INSERT INTO rows(v) VALUES(?)', [payload(bytes)]);
    },
    /** Increment a counter and record it, in one transaction; returns it. */
    count: (ctx: restate.ObjectContext) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        await db.execute('INSERT INTO counter VALUES(0, 0) ON CONFLICT(id) DO UPDATE SET n = n + 1');
        const [n] = await row<[number]>(db, 'SELECT n FROM counter WHERE id = 0');
        await db.execute('INSERT INTO history(n) VALUES(?)', [n]);
        return n;
      }),
    /** Insert `rows` rows of `bytes` bytes each, in one transaction. */
    bulk: (ctx: restate.ObjectContext, [rows, bytes]: [number, number]) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(SCHEMA);
        for (let i = 0; i < rows; i++) await db.execute('INSERT INTO rows(v) VALUES(?)', [payload(bytes)]);
      }),
    /** Rewrite row `id` with a new value of about the same length. */
    update: async (ctx: restate.ObjectContext, id: number) => {
      const [length] = await row<[number]>(ctx.db, 'SELECT length(v) FROM rows WHERE id = ?', [id]);
      await ctx.db.execute('UPDATE rows SET v = ? WHERE id = ?', [payload(length), id]);
    },
    /** Row `id`'s length, from the exclusive handler: the owner's session. */
    get: async (ctx: restate.ObjectContext, id: number) =>
      (await row<[number]>(ctx.db, 'SELECT length(v) FROM rows WHERE id = ?', [id]))[0],
    /** Row `id`'s length, from a shared handler: a reader's session. */
    peek: restate.handlers.object.shared(
      async (ctx: restate.ObjectSharedContext, id: number) =>
        (await row<[number]>(ctx.db, 'SELECT length(v) FROM rows WHERE id = ?', [id]))[0],
    ),
    /** The rows' count and total length, from a shared handler. */
    read: restate.handlers.object.shared((ctx: restate.ObjectSharedContext) =>
      row<[number, number]>(ctx.db, 'SELECT count(*), coalesce(sum(length(v)), 0) FROM rows'),
    ),
  },
  options: { retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 } },
});

const PORT = 9190;
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

if (process.argv[2] === 'serve') {
  await restate.serve({ services: [bench], port: PORT });
} else if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await drive(process.argv[2]);
}

/** One phase's numbers. */
interface Phase {
  phase: string;
  calls: number;
  concurrency: number;
  rps: number;
  p50: number;
  p90: number;
  p99: number;
  errors: number;
  stateKeys: number;
  stateBytes: number;
}

async function until(what: string, ok: () => Promise<boolean>) {
  for (const deadline = Date.now() + 60_000; !(await ok().catch(() => false)); await sleep(200)) {
    if (Date.now() > deadline) throw new Error(`${what}: timed out`);
  }
}

async function retried<T>(f: () => Promise<T>): Promise<T> {
  for (let attempt = 1; ; attempt++) {
    try {
      return await f();
    } catch (error) {
      if (attempt === 20) throw error;
      await sleep(500);
    }
  }
}

/** The objects' stored state, or one object's: its keys and bytes. */
async function stored(key?: string) {
  const query =
    "SELECT count(*) AS keys, sum(length(encode(value, 'hex'))) / 2 AS bytes FROM state WHERE service_name = 'SqlBench'" +
    (key ? ` AND service_key = '${key}'` : '');
  const response = await fetch(`${ADMIN}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({ query }),
  });
  const { rows, message } = (await response.json()) as {
    rows?: { keys: number; bytes: number | null }[];
    message?: string;
  };
  if (!rows) throw new Error(`state query: ${message}`);
  return { stateKeys: rows[0]?.keys ?? 0, stateBytes: rows[0]?.bytes ?? 0 };
}

/**
 * Call `handler` `calls` times, `concurrency` at once, on the key `key(i)`,
 * with `input`, or `input(i)` if it is a function. The state it reports is
 * all the objects', or only `stateOf`'s.
 */
async function phase(
  name: string,
  handler: string,
  input: unknown,
  calls: number,
  concurrency: number,
  key: (i: number) => string,
  stateOf?: string,
): Promise<Phase> {
  let next = 0;
  let errors = 0;
  const latencies: number[] = [];
  const body = (i: number) => JSON.stringify(typeof input === 'function' ? input(i) : input);
  const worker = async () => {
    for (let i = next++; i < calls; i = next++) {
      const started = performance.now();
      const response = await fetch(`${INGRESS}/SqlBench/${key(i)}/${handler}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: body(i),
      });
      await response.text();
      if (response.ok) latencies.push(performance.now() - started);
      else errors++;
    }
  };
  const started = performance.now();
  await Promise.all(Array.from({ length: concurrency }, worker));
  const seconds = (performance.now() - started) / 1000;
  latencies.sort((a, b) => a - b);
  const at = (q: number) =>
    +(latencies[Math.min(latencies.length - 1, Math.floor(q * latencies.length))] ?? NaN).toFixed(1);
  const result = {
    phase: name,
    calls,
    concurrency,
    rps: +(latencies.length / seconds).toFixed(1),
    p50: at(0.5),
    p90: at(0.9),
    p99: at(0.99),
    errors,
    // A partition just started may refuse the query for a moment.
    ...(await retried(() => stored(stateOf))),
  };
  console.log(JSON.stringify(result));
  return result;
}

async function drive(out?: string) {
  const server = process.env.RESTATE_SERVER;
  if (!server) throw new Error('RESTATE_SERVER is not set');
  const dir = mkdtempSync(join(tmpdir(), 'sql-bench-'));
  const log = openSync(join(dir, 'server.log'), 'a');
  const restateServer = spawn(server, ['--base-dir', join(dir, 'server')], {
    env: { ...process.env, RESTATE_BIND_IP: '127.0.0.1', RESTATE_LISTEN_MODE: 'tcp', RESTATE_LOG_FILTER: 'warn' },
    stdio: ['ignore', log, log],
  });
  let objects: ChildProcess | undefined;
  const start = async () => {
    objects?.kill('SIGKILL');
    objects = spawn(process.execPath, [fileURLToPath(import.meta.url), 'serve'], { stdio: ['ignore', log, log] });
    // The endpoint speaks HTTP/2: wait until it accepts connections.
    await until(
      'objects',
      () =>
        new Promise((resolve) => {
          const socket = connect(PORT, '127.0.0.1', () => {
            socket.destroy();
            resolve(true);
          });
          socket.on('error', () => resolve(false));
        }),
    );
  };
  // Leave nothing running, and no server data (a run stores some 200 MB).
  process.on('exit', () => {
    objects?.kill('SIGKILL');
    restateServer.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
  });
  await until('restate-server', async () => (await fetch(`${ADMIN}/health`)).ok);
  await start();
  await until('registration', async () => {
    const body = JSON.stringify({ uri: `http://127.0.0.1:${PORT}`, force: true });
    return (
      await fetch(`${ADMIN}/deployments`, { method: 'POST', headers: { 'content-type': 'application/json' }, body })
    ).ok;
  });

  const run = Math.random().toString(36).slice(2, 8);
  const phases: Phase[] = [];
  // A new database per call: open on an empty namespace, create, insert.
  phases.push(await phase('insert, new key, 32 at once', 'insert', 512, 2000, 32, (i) => `${run}-new-${i}`));
  // One database, one call at a time: the owner's warm path.
  phases.push(await phase('count, one key, sequential', 'count', null, 500, 1, () => `${run}-hot`));
  // 16 databases growing: new chunks, and deltas that rebase as they go.
  phases.push(
    await phase('insert 2 KiB, 16 keys, 16 at once', 'insert', 2048, 4000, 16, (i) => `${run}-grow-${i % 16}`),
  );
  // Large transactions: many pages each.
  phases.push(await phase('bulk 200 rows, one key, sequential', 'bulk', [200, 200], 50, 1, () => `${run}-bulk`));
  // Reads from a shared handler, warm, then after a restart of the objects.
  phases.push(await phase('read, 16 keys, warm', 'read', null, 1600, 16, (i) => `${run}-grow-${i % 16}`));
  await start();
  phases.push(await phase('read, 16 keys, cold process', 'read', null, 16, 16, (i) => `${run}-grow-${i % 16}`));
  phases.push(await phase('count, one key, cold process', 'count', null, 1, 1, () => `${run}-hot`));

  // Databases of 1, 10 and 100 MB, of 1 KiB rows, loaded 1 MB per call.
  const SIZES = [1, 10, 100];
  const ROWS_PER_MB = 1000;
  const sized = (mb: number) => `${run}-${mb}mb`;
  const anyRow = (mb: number) => () => 1 + Math.floor(Math.random() * mb * ROWS_PER_MB);
  for (const mb of SIZES) {
    const [key, rowId] = [() => sized(mb), anyRow(mb)];
    phases.push(await phase(`${mb} MB: load, 1 MB per call`, 'bulk', [ROWS_PER_MB, 1024], mb, 1, key, key()));
    phases.push(await phase(`${mb} MB: update a row, warm`, 'update', rowId, 200, 1, key, key()));
    phases.push(await phase(`${mb} MB: read a row, warm`, 'get', rowId, 200, 1, key, key()));
    phases.push(await phase(`${mb} MB: read a row, shared handler`, 'peek', rowId, 50, 1, key, key()));
  }
  // The first calls after a restart of the objects: an empty page cache. A
  // call to another key first opens the server's connection to them.
  for (let round = 1; round <= 5; round++) {
    await start();
    await phase('warm-up after a restart', 'count', null, 1, 1, () => `${run}-hot`);
    for (const mb of SIZES) {
      const [key, rowId] = [() => sized(mb), anyRow(mb)];
      phases.push(await phase(`${mb} MB: read a row, cold process (${round})`, 'get', rowId, 1, 1, key, key()));
      phases.push(
        await phase(`${mb} MB: update a row, after a cold read (${round})`, 'update', rowId, 1, 1, key, key()),
      );
    }
  }
  if (out) writeFileSync(out, JSON.stringify(phases, null, 2));
  process.exit(0);
}

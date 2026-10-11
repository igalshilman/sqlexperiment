// The context without journal replay, against a real Restate server: its
// operations are plain promises, awaited at once and combined with native or
// hand-written combinators; an operation starts when it is made; a suspension
// waits for any of them, and a cancellation reaches all of them. A handler
// that replays its journal keeps RestatePromises.
//
//   RESTATE_SERVER=/path/to/restate-server node concurrency.mjs   (or make concurrency-test)
import * as restate from '@restatedev/restate-sdk';
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import assert from 'node:assert/strict';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PORT = 9184;
const INACTIVITY = 1000; // the server's inactivity timeout: a handler idle this long suspends
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --- Services

// A handler that replays its journal (the default): a slow, keyed helper.
const helper = restate.object({
  name: 'Helper',
  handlers: {
    slow: async (ctx, { ms, value, fail }) => {
      await ctx.sleep(ms);
      if (fail) throw new restate.TerminalError(fail);
      return value;
    },
  },
});

const replayed = restate.service({
  name: 'Replayed',
  handlers: {
    combine: async (ctx) => {
      const p = ctx.sleep(10);
      await restate.RestatePromise.all([p, ctx.sleep(20)]);
      return { map: typeof p.map };
    },
  },
});

const attempts = new Map();
const attempt = (ctx) => {
  const id = ctx.request().id;
  attempts.set(id, (attempts.get(id) ?? 0) + 1);
  return () => attempts.get(id);
};
// A call to the helper, idempotent across attempts: a retried or resumed
// attempt reaches the call the first one made.
const slow = (ctx, key, args) =>
  ctx.objectClient(helper, key).slow(args, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${key}` }));

const conc = restate.object({
  name: 'Concurrency',
  handlers: {
    plain: async (ctx) => {
      const p = ctx.sleep(10);
      const call = slow(ctx, 'plain', { ms: 10, value: 1 });
      await Promise.all([p, call]);
      return {
        promise: p instanceof Promise,
        map: typeof p.map,
        orTimeout: typeof call.orTimeout,
        date: typeof (await ctx.date.now()),
        journalReplay: ctx.journalReplay,
      };
    },

    all: async (ctx) => {
      const n = attempt(ctx);
      const [, , h] = await Promise.all([
        ctx.sleep(300),
        ctx.sleep(500),
        slow(ctx, 'all', { ms: 400, value: 'h' }),
      ]);
      return { h, attempts: n() };
    },

    race: async (ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        slow(ctx, 'race', { ms: 5000, value: 'call' }),
        ctx.sleep(200).then(() => 'timeout'),
      ]);
      return { winner, attempts: n() };
    },

    custom: async (ctx) => {
      // The first two to finish, in order.
      const firstTwo = (promises) =>
        new Promise((resolve, reject) => {
          const done = [];
          for (const p of promises) {
            p.then((v) => {
              done.push(v);
              if (done.length === 2) resolve(done);
            }, reject);
          }
        });
      return firstTwo([
        slow(ctx, 'c1', { ms: 900, value: 'c' }),
        slow(ctx, 'c2', { ms: 100, value: 'a' }),
        slow(ctx, 'c3', { ms: 400, value: 'b' }),
      ]);
    },

    eagerRun: async (ctx) => {
      let ran = false;
      ctx.run('flag', () => {
        ran = true;
      }); // not awaited
      await ctx.sleep(200);
      return ran;
    },

    lateRegistration: async (ctx) => {
      const n = attempt(ctx);
      ctx.sleep(30_000); // pending all along, never awaited
      await sleep(100); // not a Restate promise
      const value = await ctx.run('late', () => 'ran');
      return { value, attempts: n() };
    },

    failures: async (ctx) => {
      ctx.run('boom', () => {
        throw new restate.TerminalError('boom');
      }); // fails, not awaited: no unhandled rejection
      const reason = await slow(ctx, 'fail', { ms: 10, fail: 'nope' }).catch((e) => e.message);
      await ctx.sleep(50);
      return reason;
    },

    ids: async (ctx) => {
      const call = slow(ctx, 'ids', { ms: 10, value: 1 });
      const send = ctx
        .objectSendClient(helper, 'ids-send')
        .slow({ ms: 10, value: 1 }, restate.rpc.sendOpts({ idempotencyKey: `${ctx.request().id}:send` }));
      const [callId, sendId, value] = await Promise.all([call.invocationId, send.invocationId, call]);
      return { call: callId.startsWith('inv'), send: sendId.startsWith('inv'), value };
    },

    // Idle past the inactivity timeout: the invocation suspends, and resumes
    // (from the start) on the first of the two to complete.
    suspend: async (ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        ctx.sleep(20_000).then(() => 'sleep'),
        slow(ctx, 'suspend', { ms: 3000, value: 'call' }),
      ]);
      return { winner, attempts: n() };
    },

    cancelled: async (ctx) => {
      const results = await Promise.allSettled([
        ctx.sleep(20_000),
        ctx.sleep(25_000),
        slow(ctx, 'cancelled', { ms: 20_000, value: 1 }),
      ]);
      return results.map((r) => (r.status === 'rejected' ? `rejected:${r.reason?.code}` : r.status));
    },
  },
  options: { journalReplay: false },
});

const flow = restate.workflow({
  name: 'Flow',
  handlers: {
    run: async (ctx) => {
      const got = ctx.promise('approval').get();
      return { promise: got instanceof Promise, map: typeof got.map, value: await got };
    },
    approve: async (ctx, value) => {
      const resolved = ctx.promise('approval').resolve(value);
      await resolved;
      return typeof resolved.map;
    },
  },
  options: { journalReplay: false },
});

const db = restate.object({
  name: 'Db',
  handlers: {
    concurrent: async (ctx) =>
      ctx.sqlSession(async (db) => {
        await db.execute('CREATE TABLE IF NOT EXISTS t (v INTEGER)');
        const [, h, r] = await Promise.all([
          db.execute('INSERT INTO t VALUES (1)'),
          slow(ctx, 'db', { ms: 100, value: 2 }),
          ctx.run('r', () => 3),
          db.query('SELECT count(*) FROM t'),
        ]);
        const [[count]] = await db.query('SELECT count(*) FROM t');
        return { h, r, count };
      }),
  },
  options: { ...restate.sqlOptions },
});

// --- Harness

const dir = mkdtempSync(join(tmpdir(), 'sqlite-kv-concurrency-'));
console.error(`logs in ${dir}`);
const server = spawn(SERVER, ['--base-dir', join(dir, 'server')], {
  env: {
    ...process.env,
    RESTATE_BIND_IP: '127.0.0.1',
    RESTATE_LISTEN_MODE: 'tcp',
    RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT: `${INACTIVITY}ms`,
  },
  stdio: ['ignore', openSync(join(dir, 'server.log'), 'a'), openSync(join(dir, 'server.log'), 'a')],
});
process.on('exit', () => server.kill('SIGKILL'));

for (const deadline = Date.now() + 60_000; ; await sleep(200)) {
  if (await fetch(`${ADMIN}/health`).then((r) => r.ok, () => false)) break;
  if (Date.now() > deadline) throw new Error('restate-server did not start');
}
const endpoint = await restate.endpoint().bind(helper).bind(replayed).bind(conc).bind(flow).bind(db).listen(PORT);
void endpoint;
for (const deadline = Date.now() + 60_000; ; await sleep(200)) {
  const registered = await fetch(`${ADMIN}/deployments`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ uri: `http://127.0.0.1:${PORT}`, force: true }),
  }).then((r) => r.ok, () => false);
  if (registered) break;
  if (Date.now() > deadline) throw new Error('registration failed');
}

async function call(path, body = null) {
  const started = Date.now();
  const r = await fetch(`${INGRESS}/${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(30_000),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(`${path}: ${r.status} ${text}`);
  return { value: JSON.parse(text), ms: Date.now() - started };
}

let failed = 0;
async function check(name, body) {
  try {
    await body();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.message}`);
  }
}

await check('operations are plain promises; replayed handlers keep RestatePromises', async () => {
  const { value } = await call('Concurrency/plain/plain');
  assert.deepEqual(value, {
    promise: true,
    map: 'undefined',
    orTimeout: 'undefined',
    date: 'number',
    journalReplay: false,
  });
  assert.deepEqual((await call('Replayed/combine')).value, { map: 'function' });
});

await check('Promise.all over sleeps and a call, in one attempt', async () => {
  const { value, ms } = await call('Concurrency/all/all');
  assert.deepEqual(value, { h: 'h', attempts: 1 });
  assert.ok(ms < 2000, `${ms} ms`);
});

await check('Promise.race: a timer beats a slow call', async () => {
  const { value, ms } = await call('Concurrency/race/race');
  assert.deepEqual(value, { winner: 'timeout', attempts: 1 });
  assert.ok(ms < 2000, `${ms} ms`);
});

await check('a hand-written combinator', async () => {
  assert.deepEqual((await call('Concurrency/custom/custom')).value, ['a', 'b']);
});

await check('ctx.run starts without an await', async () => {
  assert.equal((await call('Concurrency/eager/eagerRun')).value, true);
});

await check('a run awaited late is not held up by a pending 30s sleep', async () => {
  const { value, ms } = await call('Concurrency/late/lateRegistration');
  assert.deepEqual(value, { value: 'ran', attempts: 1 });
  assert.ok(ms < 2000, `${ms} ms`);
});

await check('failures reach their own promise; an unawaited one is harmless', async () => {
  assert.equal((await call('Concurrency/failures/failures')).value, 'nope');
});

await check('invocation ids of a call and a send', async () => {
  assert.deepEqual((await call('Concurrency/ids/ids')).value, { call: true, send: true, value: 1 });
});

await check('a suspension resumes on the first of two pending operations', async () => {
  const { value, ms } = await call('Concurrency/suspend/suspend');
  assert.equal(value.winner, 'call');
  assert.ok(value.attempts >= 2, `attempts ${value.attempts}`); // it did suspend
  assert.ok(ms < 8000, `${ms} ms`); // the call (3s), not the sleep (20s)
});

await check('a cancellation reaches every pending operation', async () => {
  const sent = await fetch(`${INGRESS}/Concurrency/cancel/cancelled/send`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: 'null',
  }).then((r) => r.json());
  await sleep(500);
  const cancel = await fetch(`${ADMIN}/invocations/${sent.invocationId}/cancel`, { method: 'PATCH' });
  assert.ok(cancel.ok, `cancel: ${cancel.status} ${await cancel.text()}`);
  const output = await fetch(`${INGRESS}/restate/invocation/${sent.invocationId}/attach`, {
    signal: AbortSignal.timeout(30_000),
  }).then((r) => r.json());
  assert.deepEqual(output, ['rejected:409', 'rejected:409', 'rejected:409']);
});

await check("a workflow's durable promise is a plain promise", async () => {
  const sent = await fetch(`${INGRESS}/Flow/f/run/send`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: 'null',
  }).then((r) => r.json());
  await sleep(200);
  assert.equal((await call('Flow/f/approve', 'yes')).value, 'undefined');
  const output = await fetch(`${INGRESS}/restate/invocation/${sent.invocationId}/attach`, {
    signal: AbortSignal.timeout(30_000),
  }).then((r) => r.json());
  assert.deepEqual(output, { promise: true, map: 'undefined', value: 'yes' });
});

await check('ctx.sqlSession with statements, a call and a run at once', async () => {
  assert.deepEqual((await call('Db/d/concurrent')).value, { h: 2, r: 3, count: 1 });
});

console.log(failed === 0 ? 'all passed' : `${failed} failed`);
process.exit(failed === 0 ? 0 : 1);

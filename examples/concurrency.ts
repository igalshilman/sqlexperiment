// The context without journal replay, against a real Restate server: its
// operations are plain promises, awaited at once and combined with native or
// hand-written combinators; an operation starts when it is made; a suspension
// waits for any of them, and a cancellation reaches all of them; there is no
// ctx.run. A handler that replays its journal keeps RestatePromises.
//
//   RESTATE_SERVER=/path/to/restate-server node concurrency.ts   (or make concurrency-test)
import * as restate from '@restatedev/restate-sdk';
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { row } from './node/sql.ts';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PORT = 9184;
const INACTIVITY = 1000; // the server's inactivity timeout: a handler idle this long suspends
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** The runtime type of `value`'s member `name`, which its static type may not have. */
const member = (value: unknown, name: string) => typeof (value as Record<string, unknown>)[name];

type Ctx = restate.SqlObjectContext;
/** What the slow helper does: wait `ms`, then return `value` or fail with `fail`. */
interface Slow {
  ms: number;
  value?: unknown;
  fail?: string;
}

// --- Services

/** A hand-written combinator: the first two of `promises` to finish, in order. */
const firstTwo = <T>(promises: Promise<T>[]) =>
  new Promise<T[]>((resolve, reject) => {
    const done: T[] = [];
    // Built on then(), on purpose.
    /* oxlint-disable promise/always-return, promise/no-callback-in-promise */
    for (const p of promises) {
      p.then((v) => {
        done.push(v);
        if (done.length === 2) resolve(done);
      }, reject);
    }
    /* oxlint-enable promise/always-return, promise/no-callback-in-promise */
  });

// A handler that replays its journal (the default): a slow, keyed helper.
const helper = restate.object({
  name: 'Helper',
  handlers: {
    slow: async (ctx: restate.ObjectContext, { ms, value, fail }: Slow) => {
      reached.add(ctx.key);
      await ctx.sleep(ms);
      if (fail) throw new restate.TerminalError(fail);
      return value;
    },
  },
});

const replayed = restate.service({
  name: 'Replayed',
  handlers: {
    combine: async (ctx: restate.Context) => {
      const p = ctx.sleep(10);
      await restate.RestatePromise.all([p, ctx.sleep(20)]);
      return { map: member(p, 'map') };
    },
  },
});

const attempts = new Map<string, number>();
// The keys of the helpers reached: the services run in this process.
const reached = new Set<string>();
const attempt = (ctx: Ctx) => {
  const id = ctx.request().id;
  attempts.set(id, (attempts.get(id) ?? 0) + 1);
  return () => attempts.get(id);
};
// A call to the helper, idempotent across attempts: a retried or resumed
// attempt reaches the call the first one made.
const slow = (ctx: Ctx, key: string, args: Slow) =>
  ctx.objectClient(helper, key).slow(args, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:${key}` }));

const conc = restate.object({
  name: 'Concurrency',
  handlers: {
    plain: async (ctx: Ctx) => {
      const p = ctx.sleep(10);
      const helperCall = slow(ctx, 'plain', { ms: 10, value: 1 });
      await Promise.all([p, helperCall]);
      return {
        promise: p instanceof Promise,
        map: member(p, 'map'),
        orTimeout: member(helperCall, 'orTimeout'),
        date: typeof (await ctx.date.now()),
        journalReplay: ctx.journalReplay,
      };
    },

    all: async (ctx: Ctx) => {
      const n = attempt(ctx);
      const [, , h] = await Promise.all([ctx.sleep(300), ctx.sleep(500), slow(ctx, 'all', { ms: 400, value: 'h' })]);
      return { h, attempts: n() };
    },

    race: async (ctx: Ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        slow(ctx, 'race', { ms: 5000, value: 'call' }),
        ctx.sleep(200).then(() => 'timeout'),
      ]);
      return { winner, attempts: n() };
    },

    custom: async (ctx: Ctx) => {
      return firstTwo([
        slow(ctx, 'c1', { ms: 900, value: 'c' }),
        slow(ctx, 'c2', { ms: 100, value: 'a' }),
        slow(ctx, 'c3', { ms: 400, value: 'b' }),
      ]);
    },

    eagerCall: async (ctx: Ctx) => {
      const key = `eager-${ctx.request().id}`;
      void slow(ctx, key, { ms: 10, value: 1 }); // not awaited
      await ctx.sleep(500);
      return reached.has(key);
    },

    noRun: async (ctx: Ctx) => {
      // The context's type has no `run`: the runtime refuses it too.
      try {
        void (ctx as unknown as restate.Context).run('r', () => 1);
        return 'ran';
      } catch (e) {
        return { terminal: e instanceof restate.TerminalError, message: (e as Error).message };
      }
    },

    uncaughtRun: async (ctx: Ctx) => (ctx as unknown as restate.Context).run('r', () => 1),

    lateRegistration: async (ctx: Ctx) => {
      const n = attempt(ctx);
      void ctx.sleep(30_000); // pending all along, never awaited
      await sleep(100); // not a Restate promise
      const value = await slow(ctx, 'late', { ms: 10, value: 'ran' });
      return { value, attempts: n() };
    },

    failures: async (ctx: Ctx) => {
      void slow(ctx, 'boom', { ms: 10, fail: 'boom' }); // fails, not awaited: no unhandled rejection
      const reason = await slow(ctx, 'fail', { ms: 10, fail: 'nope' }).catch((e: Error) => e.message);
      await ctx.sleep(50);
      return reason;
    },

    ids: async (ctx: Ctx) => {
      const helperCall = slow(ctx, 'ids', { ms: 10, value: 1 });
      const send = ctx
        .objectSendClient(helper, 'ids-send')
        .slow({ ms: 10, value: 1 }, restate.rpc.sendOpts({ idempotencyKey: `${ctx.request().id}:send` }));
      const [callId, sendId, value] = await Promise.all([helperCall.invocationId, send.invocationId, helperCall]);
      return { call: callId.startsWith('inv'), send: sendId.startsWith('inv'), value };
    },

    // Idle past the inactivity timeout: the invocation suspends, and resumes
    // (from the start) on the first of the two to complete.
    suspend: async (ctx: Ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        ctx.sleep(20_000).then(() => 'sleep'),
        slow(ctx, 'suspend', { ms: 3000, value: 'call' }),
      ]);
      return { winner, attempts: n() };
    },

    cancelled: async (ctx: Ctx) => {
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
    run: async (ctx: restate.SqlWorkflowContext) => {
      const got = ctx.promise<string>('approval').get();
      return { promise: got instanceof Promise, map: member(got, 'map'), value: await got };
    },
    approve: async (ctx: restate.SqlWorkflowSharedContext, value: string) => {
      const resolved = ctx.promise<string>('approval').resolve(value);
      await resolved;
      return member(resolved, 'map');
    },
  },
  options: { journalReplay: false },
});

const database = restate.object({
  name: 'Db',
  handlers: {
    concurrent: async (ctx: Ctx) => {
      const db = ctx.db;
      await db.execute('CREATE TABLE IF NOT EXISTS t (v INTEGER)');
      const [, h, r] = await Promise.all([
        db.execute('INSERT INTO t VALUES (1)'),
        slow(ctx, 'db', { ms: 100, value: 2 }),
        ctx.sleep(10).then(() => 3),
        db.query('SELECT count(*) FROM t'),
      ]);
      const [count] = await row<[number]>(db, 'SELECT count(*) FROM t');
      return { h, r, count };
    },

    // ctx.transaction and ctx.db share the attempt's session: two transactions
    // started at once run one after the other, and ctx.db sees both.
    shared: async (ctx: Ctx) => {
      await ctx.transaction(({ db }) => db.execute('CREATE TABLE IF NOT EXISTS u (v INTEGER)'));
      const insert = () =>
        ctx.transaction(async ({ db }) => {
          const [n] = await row<[number]>(db, 'SELECT count(*) FROM u');
          await db.execute('INSERT INTO u VALUES (?)', [n]);
          return n;
        });
      const seen = await Promise.all([insert(), insert()]);
      const [count] = await row<[number]>(ctx.db, 'SELECT count(*) FROM u');
      return { seen, count };
    },

    nested: async (ctx: Ctx) => {
      try {
        await ctx.transaction(() => ctx.db.query('SELECT 1'));
        return 'ran';
      } catch (e) {
        return { terminal: e instanceof restate.TerminalError, message: (e as Error).message };
      }
    },

    sqlError: async (ctx: Ctx) => ctx.db.execute('SELECT * FROM missing'),

    // A transaction's sends reach their target once it commits.
    sends: async (ctx: Ctx, n: number) =>
      ctx.transaction(async ({ db, tx }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS sent (n INTEGER)');
        await db.execute('INSERT INTO sent VALUES (?)', [n]);
        for (let i = 0; i < n; i++) {
          tx.genericSend({ service: 'Db', method: 'mark', key: 'marks', parameter: i, inputSerde: restate.serde.json });
        }
        return n;
      }),
    mark: async (ctx: Ctx, i: number) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS marks (i INTEGER)');
        await db.execute('INSERT INTO marks VALUES (?)', [i]);
      }),
    marks: async (ctx: Ctx) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS marks (i INTEGER)');
        return (await db.query('SELECT i FROM marks ORDER BY i')).map(([i]) => i);
      }),
  },
  options: { ...restate.sqlOptions },
});

// --- Harness

const dir = mkdtempSync(join(tmpdir(), 'sqlite-kv-concurrency-'));
console.error(`logs in ${dir}`);
// A server left running (by a run that was killed) would serve this run its
// state: refuse to start against it.
if (
  await fetch(`${ADMIN}/health`).then(
    () => true,
    () => false,
  )
) {
  throw new Error(`a Restate server already listens at ${ADMIN}; stop it first`);
}
const server = spawn(SERVER, ['--base-dir', join(dir, 'server')], {
  env: {
    ...process.env,
    RESTATE_BIND_IP: '127.0.0.1',
    RESTATE_LISTEN_MODE: 'tcp',
    RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT: `${INACTIVITY}ms`,
  },
  stdio: ['ignore', openSync(join(dir, 'server.log'), 'a'), openSync(join(dir, 'server.log'), 'a')],
});
// On failure, or when interrupted or terminated, leave nothing running.
for (const signal of ['SIGINT', 'SIGTERM'] as const) process.on(signal, () => process.exit(1));
process.on('exit', () => server.kill('SIGKILL'));

for (const deadline = Date.now() + 60_000; ; await sleep(200)) {
  if (
    await fetch(`${ADMIN}/health`).then(
      (r) => r.ok,
      () => false,
    )
  )
    break;
  if (Date.now() > deadline) throw new Error('restate-server did not start');
}
const endpoint = await restate.endpoint().bind(helper).bind(replayed).bind(conc).bind(flow).bind(database).listen(PORT);
void endpoint;
for (const deadline = Date.now() + 60_000; ; await sleep(200)) {
  const registered = await fetch(`${ADMIN}/deployments`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ uri: `http://127.0.0.1:${PORT}`, force: true }),
  }).then(
    (r) => r.ok,
    () => false,
  );
  if (registered) break;
  if (Date.now() > deadline) throw new Error('registration failed');
}

async function call(path: string, body: unknown = null): Promise<{ value: unknown; ms: number }> {
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

/** Start `path` without waiting for its result: its invocation id. */
async function start(path: string): Promise<string> {
  const r = await fetch(`${INGRESS}/${path}/send`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: 'null',
  });
  return ((await r.json()) as { invocationId: string }).invocationId;
}

let failed = 0;
async function check(name: string, body: () => Promise<void>) {
  try {
    await body();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${(e as Error).message}`);
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

await check('an operation starts without an await', async () => {
  assert.equal((await call('Concurrency/eager/eagerCall')).value, true);
});

await check('ctx.run is refused, with a terminal error', async () => {
  assert.deepEqual((await call('Concurrency/norun/noRun')).value, {
    terminal: true,
    message: 'ctx.run is not available without journal replay: call the function directly',
  });
  const started = Date.now();
  await assert.rejects(call('Concurrency/uncaught/uncaughtRun'), /not available without journal replay/);
  assert.ok(Date.now() - started < 5000, 'retried'); // terminal: not retried
});

await check('a call awaited late is not held up by a pending 30s sleep', async () => {
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
  const { value: answer, ms } = await call('Concurrency/suspend/suspend');
  const value = answer as { winner: string; attempts: number };
  assert.equal(value.winner, 'call');
  assert.ok(value.attempts >= 2, `attempts ${value.attempts}`); // it did suspend
  assert.ok(ms < 8000, `${ms} ms`); // the call (3s), not the sleep (20s)
});

await check('a cancellation reaches every pending operation', async () => {
  const invocationId = await start('Concurrency/cancel/cancelled');
  await sleep(500);
  const cancel = await fetch(`${ADMIN}/invocations/${invocationId}/cancel`, { method: 'PATCH' });
  assert.ok(cancel.ok, `cancel: ${cancel.status} ${await cancel.text()}`);
  const output = await fetch(`${INGRESS}/restate/invocation/${invocationId}/attach`, {
    signal: AbortSignal.timeout(30_000),
  }).then((r) => r.json());
  assert.deepEqual(output, ['rejected:409', 'rejected:409', 'rejected:409']);
});

await check("a workflow's durable promise is a plain promise", async () => {
  const invocationId = await start('Flow/f/run');
  await sleep(200);
  assert.equal((await call('Flow/f/approve', 'yes')).value, 'undefined');
  const output = await fetch(`${INGRESS}/restate/invocation/${invocationId}/attach`, {
    signal: AbortSignal.timeout(30_000),
  }).then((r) => r.json());
  assert.deepEqual(output, { promise: true, map: 'undefined', value: 'yes' });
});

await check('ctx.db with statements, a call and a sleep at once', async () => {
  assert.deepEqual((await call('Db/d/concurrent')).value, { h: 2, r: 3, count: 1 });
});

await check('ctx.transaction and ctx.db share one session; transactions queue', async () => {
  assert.deepEqual((await call('Db/s/shared')).value, { seen: [0, 1], count: 2 });
});

await check('a call to ctx.db inside ctx.transaction is refused, with a terminal error', async () => {
  assert.deepEqual((await call('Db/n/nested')).value, {
    terminal: true,
    message: 'inside a transaction, use the transaction handle its body gets',
  });
});

await check("a transaction's sends reach their target", async () => {
  await call('Db/o/sends', 3);
  const deadline = Date.now() + 10_000;
  let marks: unknown;
  do {
    marks = (await call('Db/marks/marks')).value;
    if (JSON.stringify(marks) === '[0,1,2]') break;
    await sleep(100);
  } while (Date.now() < deadline);
  assert.deepEqual(marks, [0, 1, 2]);
});

await check('an SQL error that the handler fails with is terminal', async () => {
  const started = Date.now();
  await assert.rejects(call('Db/e/sqlError'), /no such table: missing/);
  assert.ok(Date.now() - started < 5000, 'retried'); // terminal: not retried
});

console.log(failed === 0 ? 'all passed' : `${failed} failed`);
process.exit(failed === 0 ? 0 : 1);

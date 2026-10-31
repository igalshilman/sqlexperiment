// Session objects (restate-sessions), against a real Restate server: their
// handlers run with explicit recovery (a retried attempt starts over, against
// the database as committed), and their operations are awaited concurrently,
// with native or hand-written combinators (restate-sessions' promise selector
// drives them all); a suspension waits for any of them, and a cancellation
// reaches all of them. With lazy state, every page the database reads is a
// request to the server. A wait is a call to a journaled service, `Helper`,
// which sleeps.
//
//   pnpm --filter e2e concurrency   (RESTATE_SERVER=/path/to/restate-server to use another)
import * as restate from 'restate-sessions';
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { RESTATE_SERVER as SERVER } from './restate-server.ts';

const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PORT = 9184;
const INACTIVITY = 1000; // the server's inactivity timeout: a handler idle this long suspends
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** What the slow helper does: wait `ms`, then return `value` or fail with `fail`; `tag` names the call. */
interface Slow {
  tag: string;
  ms: number;
  value?: unknown;
  fail?: string;
}

// --- Services

/** A hand-written combinator: the first two of `promises` to finish, in order. */
const firstTwo = <T>(promises: PromiseLike<T>[]) =>
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

// The calls reached, by tag: the services run in this process.
const reached = new Set<string>();

// A slow helper, a journaled service: it waits in Restate, as a timer would.
const helper = restate.service({
  name: 'Helper',
  handlers: {
    slow: async (ctx: restate.Context, { tag, ms, value, fail }: Slow) => {
      reached.add(tag);
      await ctx.sleep(ms);
      if (fail) throw new restate.TerminalError(fail);
      return value;
    },
  },
});

const attempts = new Map<string, number>();
const attempt = (ctx: restate.SessionContext | restate.ViewContext) => {
  const id = ctx.request.id;
  attempts.set(id, (attempts.get(id) ?? 0) + 1);
  return () => attempts.get(id);
};
// A call to the helper, idempotent across attempts: a retried or resumed
// attempt reaches the call the first one made.
const slow = (ctx: restate.SessionContext, args: Slow) =>
  ctx.rpc(helper, { idempotencyKey: `${ctx.request.id}:${args.tag}` }).slow(args);
/** A wait of `ms`, as a call. */
const pause = (ctx: restate.SessionContext, tag: string, ms: number) => slow(ctx, { tag, ms });

const conc = restate.sessionObject({
  name: 'Concurrency',
  schema: ['CREATE TABLE pages (i INTEGER PRIMARY KEY, v TEXT NOT NULL)', 'CREATE TABLE written (v TEXT)'],
  handlers: {
    all: async (ctx) => {
      const n = attempt(ctx);
      const [, , h] = await Promise.all([
        pause(ctx, 'a1', 300),
        pause(ctx, 'a2', 500),
        slow(ctx, { tag: 'all', ms: 400, value: 'h' }),
      ]);
      return { h, attempts: n() };
    },

    race: async (ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        slow(ctx, { tag: 'race', ms: 5000, value: 'call' }),
        pause(ctx, 'race-timeout', 200).then(() => 'timeout'),
      ]);
      return { winner, attempts: n() };
    },

    custom: async (ctx) =>
      firstTwo([
        slow(ctx, { tag: 'c1', ms: 900, value: 'c' }),
        slow(ctx, { tag: 'c2', ms: 100, value: 'a' }),
        slow(ctx, { tag: 'c3', ms: 400, value: 'b' }),
      ]),

    chains: async (ctx) => {
      const after = async (ms: number, value: string) => {
        await pause(ctx, `chain-${value}`, ms);
        return value;
      };
      return Promise.all([after(100, 'a'), after(150, 'b')]);
    },

    eagerCall: async (ctx) => {
      const tag = `eager-${ctx.request.id}`;
      void slow(ctx, { tag, ms: 10, value: 1 }); // not awaited
      await pause(ctx, 'eager-wait', 500);
      return reached.has(tag);
    },

    /** What a handler's context has: no state, timers or awakeables. */
    context: async (ctx) => Object.keys(ctx).toSorted(),

    lateRegistration: async (ctx) => {
      const n = attempt(ctx);
      void pause(ctx, 'late-pending', 30_000); // pending all along, never awaited
      await sleep(100); // not a Restate promise
      const value = await slow(ctx, { tag: 'late', ms: 10, value: 'ran' });
      return { value, attempts: n() };
    },

    failures: async (ctx) => {
      void slow(ctx, { tag: 'boom', ms: 10, fail: 'boom' }); // fails, not awaited: no unhandled rejection
      const reason = await slow(ctx, { tag: 'fail', ms: 10, fail: 'nope' }).catch((e: Error) => e.message);
      await pause(ctx, 'failures-wait', 50);
      return reason;
    },

    // Idle past the inactivity timeout: the invocation suspends, and resumes
    // (from the start) on the first of the two to complete.
    suspend: async (ctx) => {
      const n = attempt(ctx);
      const winner = await Promise.race([
        pause(ctx, 'suspend-sleep', 20_000).then(() => 'sleep'),
        slow(ctx, { tag: 'suspend', ms: 3000, value: 'call' }),
      ]);
      return { winner, attempts: n() };
    },

    cancelled: async (ctx) => {
      const results = await Promise.allSettled([
        pause(ctx, 'cancel-1', 20_000),
        pause(ctx, 'cancel-2', 25_000),
        slow(ctx, { tag: 'cancelled', ms: 20_000, value: 1 }),
      ]);
      return results.map((r) => (r.status === 'rejected' ? `rejected:${r.reason?.code}` : r.status));
    },

    // --- The database, its pages read lazily: each a request to the server.
    seed: async (ctx, n: number) =>
      ctx.db.transaction(async (tx) => {
        for (let i = 0; i < n; i++) await tx.execute('INSERT OR REPLACE INTO pages VALUES (?, ?)', [i, `v${i}`]);
      }),
    gets: async (ctx, n: number) =>
      Promise.all(
        Array.from(
          { length: n },
          async (_, i) => (await ctx.db.get<{ v: string }>('SELECT v FROM pages WHERE i = ?', [i]))?.v,
        ),
      ),
    getChains: async (ctx) => {
      // 8 async functions of 10 reads each, at once.
      const chain = async (c: number) => {
        const values: (string | undefined)[] = [];
        for (let i = 0; i < 10; i++) {
          values.push((await ctx.db.get<{ v: string }>('SELECT v FROM pages WHERE i = ?', [c * 10 + i]))?.v);
        }
        return values.join(',');
      };
      return Promise.all(Array.from({ length: 8 }, (_, c) => chain(c)));
    },
    // A retried attempt starts over, and reads what the one before committed.
    recovers: async (ctx) => {
      const n = attempt(ctx);
      const first = await ctx.db.get<{ v: string }>('SELECT v FROM pages WHERE i = 0');
      const seen = await ctx.db.get<{ v: string }>('SELECT v FROM written');
      if (n() === 1) {
        await ctx.db.execute("INSERT INTO written VALUES ('first')");
        throw new Error('retry me');
      }
      return { first: first?.v, seen: seen?.v, attempts: n() };
    },
  },
  views: {
    viewGets: async (ctx, n: number) =>
      Promise.all(
        Array.from(
          { length: n },
          async (_, i) => (await ctx.db.get<{ v: string }>('SELECT v FROM pages WHERE i = ?', [i]))?.v,
        ),
      ),
  },
});

const database = restate.sessionObject({
  name: 'Db',
  schema: ['CREATE TABLE t (v INTEGER)', 'CREATE TABLE u (v INTEGER)', 'CREATE TABLE marks (i INTEGER)'],
  handlers: {
    concurrent: async (ctx) => {
      const [, h, r] = await Promise.all([
        ctx.db.execute('INSERT INTO t VALUES (1)'),
        slow(ctx, { tag: 'db', ms: 100, value: 2 }),
        pause(ctx, 'db-wait', 10).then(() => 3),
        ctx.db.values('SELECT count(*) FROM t'),
      ]);
      const count = (await ctx.db.get<{ n: number }>('SELECT count(*) AS n FROM t'))?.n;
      return { h, r, count };
    },

    // Two transactions started at once run one after the other, and ctx.db sees both.
    shared: async (ctx) => {
      const insert = () =>
        ctx.db.transaction(async (tx) => {
          const n = (await tx.get<{ n: number }>('SELECT count(*) AS n FROM u'))!.n;
          await tx.execute('INSERT INTO u VALUES (?)', [n]);
          return n;
        });
      const seen = await Promise.all([insert(), insert()]);
      const count = (await ctx.db.get<{ n: number }>('SELECT count(*) AS n FROM u'))?.n;
      return { seen, count };
    },

    nested: async (ctx) => {
      try {
        await ctx.db.transaction(() => ctx.db.values('SELECT 1'));
        return 'ran';
      } catch (e) {
        return { terminal: e instanceof restate.TerminalError, message: (e as Error).message };
      }
    },

    sqlError: async (ctx) => ctx.db.execute('SELECT * FROM missing'),

    // A transaction's sends reach their target once it commits.
    sends: async (ctx, n: number) =>
      ctx.db.transaction(async (tx) => {
        for (let i = 0; i < n; i++) tx.send({ service: 'Db', key: 'marks', handler: 'mark', input: i });
        return n;
      }),
    mark: async (ctx, i: number) => {
      await ctx.db.execute('INSERT INTO marks VALUES (?)', [i]);
    },
  },
  views: {
    marks: async (ctx) => (await ctx.db.query<{ i: number }>('SELECT i FROM marks ORDER BY i')).map(({ i }) => i),
  },
});

// --- Harness

const dir = mkdtempSync(join(tmpdir(), 'restate-sessions-concurrency-'));
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
const endpoint = await restate.endpoint().bind(helper).bind(conc).bind(database).listen(PORT);
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
  return { value: text ? JSON.parse(text) : undefined, ms: Date.now() - started };
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

await check('two async functions, each awaiting a sleep', async () => {
  assert.deepEqual((await call('Concurrency/chains/chains')).value, ['a', 'b']);
});

await check('an operation starts without an await', async () => {
  assert.equal((await call('Concurrency/eager/eagerCall')).value, true);
});

await check("a handler's context: the key, the request, the database and calls", async () => {
  assert.deepEqual((await call('Concurrency/context/context')).value, ['db', 'key', 'request', 'rpc', 'send']);
});

await check('a call awaited late is not held up by a pending 30s sleep', async () => {
  const { value, ms } = await call('Concurrency/late/lateRegistration');
  assert.deepEqual(value, { value: 'ran', attempts: 1 });
  assert.ok(ms < 2000, `${ms} ms`);
});

await check('failures reach their own promise; an unawaited one is harmless', async () => {
  assert.equal((await call('Concurrency/failures/failures')).value, 'nope');
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

const values = (n: number) => Array.from({ length: n }, (_, i) => `v${i}`);
await call('Concurrency/state/seed', 80);

await check('40 lazy database reads at once', async () => {
  assert.deepEqual((await call('Concurrency/state/gets', 40)).value, values(40));
});

await check('8 async functions of 10 lazy database reads each, at once', async () => {
  const chains = (await call('Concurrency/state/getChains')).value as string[];
  const expected = Array.from({ length: 8 }, (_, c) =>
    values(80)
      .slice(c * 10, c * 10 + 10)
      .join(','),
  );
  assert.deepEqual(chains, expected);
});

await check('a view: 40 lazy database reads at once', async () => {
  assert.deepEqual((await call('Concurrency/state/viewGets', 40)).value, values(40));
});

await check('a retried attempt starts over, and reads what the one before committed', async () => {
  assert.deepEqual((await call('Concurrency/state/recovers')).value, { first: 'v0', seen: 'first', attempts: 2 });
});

await check('ctx.db with statements and calls at once', async () => {
  assert.deepEqual((await call('Db/d/concurrent')).value, { h: 2, r: 3, count: 1 });
});

await check('transactions started at once queue, and ctx.db sees both', async () => {
  assert.deepEqual((await call('Db/s/shared')).value, { seen: [0, 1], count: 2 });
});

await check('a call to ctx.db inside a transaction is refused, with a terminal error', async () => {
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

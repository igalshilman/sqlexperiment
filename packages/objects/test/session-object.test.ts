// Session objects without a Restate server: their definition, their
// contexts, transactions with sends, and the types users write against.
import assert from 'node:assert/strict';
import * as restate from 'restate-objects';
import { invoke, reset, Schema } from 'restate-objects/internal';
import { expectTypeOf, test, vi } from 'vitest';
import { fakeContext, type Made } from './support.ts';

const balanceOf = async (db: restate.DatabaseReader) =>
  (await db.get<{ balance: number }>('SELECT coalesce(sum(amount), 0) AS balance FROM entries'))?.balance ?? 0;

// Lesson 08: a handler that refers to its own object.
const account = restate.sessionObject({
  name: 'account',
  schema: ['CREATE TABLE entries (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)'],
  handlers: {
    deposit: async (ctx, { id, amount }: { id: string; amount: number }) => {
      await ctx.db.execute('INSERT OR IGNORE INTO entries (id, amount) VALUES (?, ?)', [id, amount]);
      return balanceOf(ctx.db);
    },
    transfer: (ctx, { to, amount }: { to: string; amount: number }) =>
      ctx.db.transaction(async (tx) => {
        const id = ctx.request.id;
        if ((await tx.get('SELECT 1 FROM entries WHERE id = ?', [id])) === undefined) {
          if ((await balanceOf(tx)) < amount) throw new restate.TerminalError('Insufficient funds');
          await tx.execute('INSERT INTO entries (id, amount) VALUES (?, ?)', [id, -amount]);
          tx.send(account, to).deposit({ id, amount });
        }
        return balanceOf(tx);
      }),
    failing: {
      description: 'Rolls back.',
      handler: async (ctx) =>
        ctx.db.transaction(async (tx) => {
          await tx.execute("INSERT INTO entries (id, amount) VALUES ('x', 1)");
          tx.send(account, 'other').deposit({ id: 'x', amount: 1 });
          throw new restate.TerminalError('no');
        }),
    },
    expire: async (ctx) => {
      ctx.send(account, ctx.key, { delay: 1000, idempotencyKey: 'e' }).balance();
    },
  },
  views: {
    balance: async (ctx) => balanceOf(ctx.db),
    contexts: async (ctx) => Object.keys(ctx).toSorted(),
  },
});

test('a handler commits, and its transaction sends once it committed', async () => {
  const map = new Map<string, Uint8Array>();
  const sends: Made[] = [];
  const ctx = (id: string) => fakeContext({ map, service: 'account', key: 'alice', id, sends });
  assert.equal(await invoke(account, 'deposit', ctx('d1'), { id: 'd1', amount: 10 }), 10);
  assert.equal(await invoke(account, 'transfer', ctx('t1'), { to: 'bob', amount: 4 }), 6);
  assert.deepEqual(sends, [
    {
      service: 'account',
      method: 'deposit',
      key: 'bob',
      parameter: { id: 't1', amount: 4 },
      idempotencyKey: sends[0]?.idempotencyKey,
    },
  ]);
  assert.match(sends[0]!.idempotencyKey!, /^transaction\/[\w-]+\/0$/);

  // A new attempt of the same transfer finds it committed: it moves nothing.
  assert.equal(await invoke(account, 'transfer', ctx('t1'), { to: 'bob', amount: 4 }), 6);
  assert.equal(sends.length, 1);
  assert.equal(await invoke(account, 'balance', ctx('v1')), 6);
});

test('a transaction that rolls back sends nothing', async () => {
  const map = new Map<string, Uint8Array>();
  const sends: Made[] = [];
  await assert.rejects(invoke(account, 'failing', fakeContext({ map, service: 'account', sends })), /no/);
  assert.deepEqual(sends, []);
  assert.equal(await invoke(account, 'balance', fakeContext({ map, service: 'account' })), 0);
});

test('a send takes its options with the target, and a no-input handler none', async () => {
  const sends: Made[] = [];
  await invoke(account, 'expire', fakeContext({ map: new Map(), service: 'account', key: 'alice', sends }));
  assert.deepEqual(sends, [
    { service: 'account', method: 'balance', key: 'alice', parameter: undefined, idempotencyKey: 'e', delay: 1000 },
  ]);
});

test('a context has the key, the request, the database and calls; a view no sends, and reads only', async () => {
  const map = new Map<string, Uint8Array>();
  assert.deepEqual(await invoke(account, 'contexts', fakeContext({ map, service: 'account' })), [
    'db',
    'key',
    'request',
    'rpc',
  ]);
  const seen = restate.sessionObject({
    name: 'seen',
    handlers: { keys: async (ctx) => ({ ctx: Object.keys(ctx).toSorted(), db: Object.keys(ctx.db).toSorted() }) },
    views: { db: async (ctx) => Object.keys(ctx.db).toSorted() },
  });
  assert.deepEqual(await invoke(seen, 'keys', fakeContext({ map, service: 'seen' })), {
    ctx: ['db', 'key', 'request', 'rpc', 'send'],
    db: ['execute', 'get', 'query', 'transaction', 'values'],
  });
  assert.deepEqual(await invoke(seen, 'db', fakeContext({ map, service: 'seen' })), ['get', 'query', 'values']);
});

test('a view of a key never written reads empty tables', async () => {
  assert.equal(await invoke(account, 'balance', fakeContext({ map: new Map(), service: 'account', key: 'new' })), 0);
});

test('calls: typed, with options with the target; and literal', async () => {
  const calls: Made[] = [];
  const caller = restate.sessionObject({
    name: 'caller',
    handlers: {
      call: async (ctx) => {
        const balance = await ctx.rpc(account, 'bob', { idempotencyKey: `${ctx.request.id}:balance` }).balance();
        const literal = await ctx.rpc<number>({ service: 'account', key: 'bob', handler: 'balance' });
        return [balance, literal];
      },
    },
  });
  const ctx = fakeContext({ map: new Map(), service: 'caller', id: 'c1', calls, answer: () => 5 });
  assert.deepEqual(await invoke(caller, 'call', ctx), [5, 5]);
  assert.deepEqual(calls, [
    { service: 'account', method: 'balance', key: 'bob', parameter: undefined, idempotencyKey: 'c1:balance' },
    { service: 'account', method: 'balance', key: 'bob', parameter: undefined },
  ]);
});

test('an object or workflow needs its key, a service has none', async () => {
  const pricing = restate.service({
    name: 'pricing',
    handlers: { quote: async (_ctx: restate.Context, sku: string) => sku.length },
  });
  const wrong = restate.sessionObject({
    name: 'wrong',
    handlers: {
      noKey: async (ctx) => (ctx.rpc as (target: unknown) => unknown)(account),
      key: async (ctx) => (ctx.rpc as (target: unknown, key: string) => unknown)(pricing, 'k'),
    },
  });
  await assert.rejects(
    invoke(wrong, 'noKey', fakeContext({ map: new Map(), service: 'wrong' })),
    /account is an object: call it with its key/,
  );
  await assert.rejects(
    invoke(wrong, 'key', fakeContext({ map: new Map(), service: 'wrong' })),
    /pricing is a service: call it without a key/,
  );
});

test('a definition refuses a name of both kinds, and SDK handlers', () => {
  assert.throws(
    () => restate.sessionObject({ name: 'twice', handlers: { a: async () => 1 }, views: { a: async () => 2 } }),
    /a is a handler and a view/,
  );
  assert.throws(
    () =>
      restate.sessionObject({
        name: 'sdk',
        handlers: { a: restate.handlers.object.exclusive(async () => 1) as never },
      }),
    /sdk\.a is an SDK handler/,
  );
});

test('types: contexts are inferred, clients typed by the definition', () => {
  const pricing = restate.service({
    name: 'pricing',
    handlers: { quote: async (_ctx: restate.Context, sku: string) => sku.length },
  });
  restate.sessionObject({
    name: 'typed',
    handlers: {
      plain: async (ctx, text: string) => {
        expectTypeOf(ctx).toEqualTypeOf<restate.SessionContext>();
        expectTypeOf(ctx.rpc(account, 'k').deposit).toEqualTypeOf<
          (input: { id: string; amount: number }) => Promise<number>
        >();
        expectTypeOf(ctx.rpc(account, 'k').balance).toEqualTypeOf<() => Promise<number>>();
        expectTypeOf(ctx.rpc(pricing).quote).toEqualTypeOf<(sku: string) => Promise<number>>();
        expectTypeOf(ctx.send(account, 'k', { delay: 5 }).deposit).toEqualTypeOf<
          (input: { id: string; amount: number }) => void
        >();
        // @ts-expect-error a wrong input
        void ctx.rpc(account, 'k').deposit('x');
        return text;
      },
      withOptions: {
        input: restate.serde.json,
        handler: async (ctx, n: number) => {
          expectTypeOf(ctx).toEqualTypeOf<restate.SessionContext>();
          return n;
        },
      },
    },
    views: {
      read: async (ctx) => {
        expectTypeOf(ctx).toEqualTypeOf<restate.ViewContext>();
        expectTypeOf(ctx.db).toEqualTypeOf<restate.DatabaseReader>();
        // @ts-expect-error a view has no sends
        void ctx.send;
        // @ts-expect-error nor writes
        void ctx.db.execute;
      },
    },
  });
});

// A crash between a commit and its sends: the attempt's sends fail as Restate fences them.
const timer = restate.sessionObject({
  name: 'timer',
  schema: ['CREATE TABLE armed (at INTEGER)'],
  handlers: {
    arm: async (ctx) =>
      ctx.db.transaction(async (tx) => {
        await tx.execute('INSERT INTO armed VALUES (1)');
        tx.send(timer, ctx.key, { delay: 60_000 }).ring();
      }),
    /** Uses no database. */
    nothing: async () => 'done',
    ring: async () => 'ring',
  },
});

test('sends a crash left unmade go out as the next attempt starts, though it uses no database', async () => {
  vi.useFakeTimers({ toFake: ['Date'] });
  try {
    vi.setSystemTime(1_000_000);
    const map = new Map<string, Uint8Array>();
    const crashing = fakeContext({ map, service: 'timer', key: 't' });
    crashing.genericSend = () => {
      throw new Error('the attempt was fenced');
    };
    await assert.rejects(invoke(timer, 'arm', crashing), /fenced/);

    // 20 s later, an attempt of a handler that never uses ctx.db makes the send, with the rest of its delay.
    vi.setSystemTime(1_020_000);
    const sends: Made[] = [];
    assert.equal(await invoke(timer, 'nothing', fakeContext({ map, service: 'timer', key: 't', sends })), 'done');
    assert.equal(sends.length, 1);
    assert.equal(sends[0]!.method, 'ring');
    assert.equal(sends[0]!.delay, 40_000);
    assert.match(sends[0]!.idempotencyKey!, /^transaction\/[\w-]+\/0$/);

    // Made, and stored: the attempt after makes it no more.
    const later: Made[] = [];
    await invoke(timer, 'nothing', fakeContext({ map, service: 'timer', key: 't', sends: later }));
    assert.deepEqual(later, []);
  } finally {
    vi.useRealTimers();
  }
});

const LIBRARY = new Schema('library', ['CREATE TABLE restate_library (v TEXT)']);

const resettable = restate.sessionObject({
  name: 'resettable',
  schema: [
    'CREATE TABLE items (v TEXT)',
    'CREATE TABLE starts (at INTEGER)',
    'CREATE VIEW counted AS SELECT count(*) AS n FROM items',
    'CREATE TRIGGER stamped AFTER INSERT ON items BEGIN INSERT INTO starts VALUES (0); END',
  ],
  handlers: {
    fill: async (ctx) => {
      await LIBRARY.ensure(ctx.db);
      await ctx.db.execute("INSERT INTO items VALUES ('a'), ('b')");
      await ctx.db.execute("INSERT INTO restate_library VALUES ('x')");
      await ctx.db.execute('CREATE TABLE extra (v TEXT)');
    },
    clear: async (ctx, start: number) => {
      await LIBRARY.ensure(ctx.db);
      await ctx.db.transaction(async (tx) => {
        tx.send(resettable, 'other').fill();
        await reset(tx);
        await tx.execute('INSERT INTO starts VALUES (?)', [start]);
      });
    },
    failing: async (ctx) =>
      ctx.db.transaction(async (tx) => {
        await reset(tx);
        throw new restate.TerminalError('rolled back');
      }),
  },
  views: {
    state: async (ctx) => ({
      items: await ctx.db.get<{ n: number }>('SELECT n FROM counted'),
      starts: await ctx.db.query('SELECT at FROM starts'),
      library: await ctx.db.query('SELECT v FROM restate_library'),
      tables: await ctx.db.query("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name"),
    }),
  },
});

test('reset empties the database in a transaction, with the schemas applied again and its sends kept', async () => {
  const map = new Map<string, Uint8Array>();
  const ctx = (sends: Made[] = []) => fakeContext({ map, service: 'resettable', sends });
  await invoke(resettable, 'fill', ctx());
  await assert.rejects(invoke(resettable, 'failing', ctx()), /rolled back/);
  assert.deepEqual(await invoke(resettable, 'state', ctx()), {
    items: { n: 2 },
    starts: [{ at: 0 }, { at: 0 }],
    library: [{ v: 'x' }],
    tables: [{ name: 'extra' }, { name: 'items' }, { name: 'restate_library' }, { name: 'starts' }],
  });

  const sends: Made[] = [];
  await invoke(resettable, 'clear', ctx(sends), 7);
  assert.deepEqual(await invoke(resettable, 'state', ctx()), {
    items: { n: 0 },
    starts: [{ at: 7 }],
    library: [],
    tables: [{ name: 'items' }, { name: 'restate_library' }, { name: 'starts' }],
  });
  assert.deepEqual(
    sends.map(({ method, key }) => [method, key]),
    [['fill', 'other']],
  );

  // The trigger is back with the schema.
  await invoke(resettable, 'fill', ctx());
  assert.deepEqual(await invoke(resettable, 'state', ctx()), {
    items: { n: 2 },
    starts: [{ at: 7 }, { at: 0 }, { at: 0 }],
    library: [{ v: 'x' }],
    tables: [{ name: 'extra' }, { name: 'items' }, { name: 'restate_library' }, { name: 'starts' }],
  });
});

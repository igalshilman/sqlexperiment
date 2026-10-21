// `ctx.transaction`'s sends, without a Restate server: each attempt has the
// SDK's `ctx.db` and `ctx.transaction` over the object's state in memory, and
// records the sends it makes. A crash is an attempt whose later writes and
// sends are lost: what it stored before stays.
import * as restate from 'restate-fns';
import { open } from 'restate-sql';
import { describe, expect, it } from 'vitest';
import { memoryStore, sqlAttempt } from '../testing.ts';

type Send = restate.GenericSend<Uint8Array>;

/** The database's commit record, whose effects hold the outbox. */
const COMMIT = 'v15/commit';
let namespaces = 0;

const store = restate.object({
  name: 'Store',
  handlers: {
    reserve: async (_ctx: restate.ObjectContext, _item: string) => {},
    deposit: async (_ctx: restate.ObjectContext, _amount: number) => {},
  },
});

/** An object's state, and attempts over it that record their sends. */
function object() {
  const map = new Map<string, Uint8Array>();
  return {
    map,
    /** An attempt; with `crash`, its writes and sends are lost from the first send on. */
    attempt({ crash = false } = {}) {
      const sent: Send[] = [];
      const fence = { closed: false };
      const send = (s: Send) => {
        if (crash) {
          fence.closed = true;
          throw new Error('crashed');
        }
        sent.push(s);
      };
      return { ...sqlAttempt({ map, fence, send }), sent, fence };
    },
    /** The keys of the sends in the stored outbox. */
    async outbox() {
      const db = await open(`outbox-${namespaces++}`, memoryStore(map));
      const effects = await db.effects();
      await db.close();
      if (effects.length === 0) return [];
      const outbox = JSON.parse(new TextDecoder().decode(effects)) as { id: string; sends: unknown[] }[];
      return outbox.flatMap(({ id, sends }) => sends.map((_, i) => `transaction/${id}/${i}`));
    },
  };
}

const decode = (s: Send) => JSON.parse(new TextDecoder().decode(s.parameter)) as unknown;

/** A transfer with two sends, as one transaction. */
const transfer = (a: ReturnType<ReturnType<typeof object>['attempt']>) =>
  a.transaction(async ({ db, tx }) => {
    const before = a.sent.length;
    await db.execute('CREATE TABLE IF NOT EXISTS balance (v INTEGER)');
    await db.execute('INSERT INTO balance VALUES (5)');
    tx.objectSendClient(store, 'a').reserve('book');
    tx.objectSendClient(store, 'b').deposit(5);
    // Nothing is sent before the commit.
    expect(a.sent).toHaveLength(before);
  });

const balance = async (o: ReturnType<typeof object>) => {
  const a = o.attempt();
  const [[count]] = (await a.end(() => a.db.query("SELECT count(*) FROM sqlite_master WHERE name = 'balance'"))) as [
    [number],
  ];
  if (!count) return 0;
  const b = o.attempt();
  return (await b.end(() => b.db.query('SELECT count(*) FROM balance')))[0]![0];
};

describe('the sends of ctx.transaction', () => {
  it('sends once the transaction commits, each with a key of the transaction', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() => transfer(a));
    expect(a.sent.map((s) => [s.service, s.key, s.method, decode(s)])).toEqual([
      ['Store', 'a', 'reserve', 'book'],
      ['Store', 'b', 'deposit', 5],
    ]);
    const [first, second] = a.sent.map((s) => s.idempotencyKey);
    expect(first).toMatch(/^transaction\/[0-9a-f-]{36}\/0$/);
    expect(second).toBe(first!.replace(/0$/, '1'));
    // Made and stored, they leave the outbox; the database has no table of them.
    expect(await o.outbox()).toEqual([]);
    const b = o.attempt();
    expect(await b.end(() => b.db.query("SELECT name FROM sqlite_master WHERE name LIKE '_restate%'"))).toEqual([]);
  });

  it('sends nothing for a transaction rolled back', async () => {
    const o = object();
    const a = o.attempt();
    const failed = await a
      .end(() =>
        a.transaction(async ({ db, tx }) => {
          await db.execute('CREATE TABLE t (v INTEGER)');
          tx.objectSendClient(store, 'a').reserve('book');
          throw new Error('no');
        }),
      )
      .catch((e: unknown) => e);
    expect((failed as Error).message).toBe('no');
    expect(a.sent).toEqual([]);
    expect(await o.outbox()).toEqual([]);
  });

  it('sends nothing for a COMMIT that is refused', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(async () => {
      await a.db.execute('PRAGMA foreign_keys=ON');
      await a.db.execute('CREATE TABLE p (id INTEGER PRIMARY KEY)');
      await a.db.execute('CREATE TABLE c (pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)');
      // The deferred constraint fails at COMMIT, after the sends wait.
      const refused = await a
        .transaction(async ({ db, tx }) => {
          await db.execute('INSERT INTO c VALUES (7)');
          tx.objectSendClient(store, 'a').reserve('book');
        })
        .catch((e: unknown) => e);
      expect(refused).toBeInstanceOf(restate.TerminalError);
    });
    expect(a.sent).toEqual([]);
    expect(await o.outbox()).toEqual([]);
  });

  it('sends again, with the same keys, what a crash after the commit lost', async () => {
    const o = object();
    const crashed = o.attempt({ crash: true });
    await expect(transfer(crashed)).rejects.toThrow('crashed'); // committed; the sends are lost
    crashed.abort();
    const lost = await o.outbox();
    expect(lost).toHaveLength(2);
    // The next attempt's first use of the database makes them: the target
    // drops a send whose key it has seen.
    const next = o.attempt();
    await next.end(() => next.db.query('SELECT 1'));
    expect(next.sent.map((s) => s.idempotencyKey)).toEqual(lost);
    expect(next.sent.map(decode)).toEqual(['book', 5]);
    expect(await o.outbox()).toEqual([]);
    expect(await balance(o)).toBe(1);
  });

  it('sends again what a crash lost of a transaction that changed nothing', async () => {
    const o = object();
    const setup = o.attempt();
    await setup.end(() => setup.db.execute('CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (1)'));
    const crashed = o.attempt({ crash: true });
    // It only reads, and its update matches no rows: still, it committed.
    const decide = crashed.transaction(async ({ db, tx }) => {
      const [[v]] = (await db.query('SELECT v FROM t')) as [[number]];
      await db.execute('UPDATE t SET v = 2 WHERE v = 99');
      if (v === 1) tx.objectSendClient(store, 'a').reserve('book');
    });
    await expect(decide).rejects.toThrow('crashed');
    crashed.abort();
    const lost = await o.outbox();
    expect(lost).toHaveLength(1);
    const next = o.attempt();
    await next.end(() => next.db.query('SELECT 1'));
    expect(next.sent.map((s) => s.idempotencyKey)).toEqual(lost);
    expect(await o.outbox()).toEqual([]);
  });

  it('drops the sends of a commit that a crash lost', async () => {
    // The commit record that holds the sends is not stored, nor anything after.
    const o = object();
    const crashed = o.attempt();
    const set = o.map.set.bind(o.map);
    o.map.set = (key, value) => {
      if (key === COMMIT) crashed.fence.closed = true;
      else set(key, value);
      return o.map;
    };
    await crashed
      .transaction(async ({ db, tx }) => {
        await db.execute('CREATE TABLE t (v INTEGER)');
        tx.objectSendClient(store, 'a').reserve('book');
      })
      .catch(() => {});
    crashed.abort();
    o.map.set = set;
    expect(await o.outbox()).toEqual([]);
    const next = o.attempt();
    expect(await next.end(() => next.db.query("SELECT count(*) FROM sqlite_master WHERE name = 't'"))).toEqual([[0]]);
    expect(next.sent).toEqual([]);
    expect(await o.outbox()).toEqual([]);
  });

  it('keeps the sends of concurrent transactions apart', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() => Promise.all([transfer(a), transfer(a), transfer(a)]));
    expect(a.sent.map(decode)).toEqual(['book', 5, 'book', 5, 'book', 5]);
    expect(new Set(a.sent.map((s) => s.idempotencyKey)).size).toBe(6);
    expect(await o.outbox()).toEqual([]);
  });

  it('writes one commit record for a transaction without sends', async () => {
    const o = object();
    const a = o.attempt();
    const sets: string[] = [];
    const set = o.map.set.bind(o.map);
    o.map.set = (key, value) => (sets.push(key), set(key, value));
    await a.end(() => a.transaction(({ db }) => db.execute('CREATE TABLE t (v INTEGER)')));
    expect(sets).toEqual([COMMIT]);
  });

  it('leaves out of the outbox the sends made before a commit', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(async () => {
      await transfer(a);
      const first = await o.outbox();
      expect(first).toHaveLength(2);
      // The first's sends are made after its commit, and before the second's:
      // Restate stores them whenever it stores that commit, which drops them.
      await transfer(a);
      const second = await o.outbox();
      expect(second).toHaveLength(2);
      expect(second).not.toContain(first[0]);
    });
    expect(await o.outbox()).toEqual([]);
  });

  it("keeps a send's delay and its own idempotency key", async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() =>
      a.transaction(async ({ tx }) => {
        tx.objectSendClient(store, 'a').reserve(
          'book',
          restate.rpc.sendOpts({ delay: { seconds: 2 }, idempotencyKey: 'mine' }),
        );
      }),
    );
    expect(a.sent.map((s) => [s.delay, s.idempotencyKey])).toEqual([[2000, 'mine']]);
  });

  it('refuses a send after the body, and has no invocation id', async () => {
    const o = object();
    const a = o.attempt();
    let late: restate.SqlTransactionSends | undefined;
    await a.end(() =>
      a.transaction(async ({ tx }) => {
        late = tx;
        const handle = tx.objectSendClient(store, 'a').reserve('book');
        expect(() => handle.invocationId).toThrow(restate.TerminalError);
      }),
    );
    expect(() => late!.objectSendClient(store, 'a').reserve('pen')).toThrow(restate.TerminalError);
    expect(a.sent.map(decode)).toEqual(['book']);
  });
});

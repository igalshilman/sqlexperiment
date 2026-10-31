// A transaction's sends (`tx.send`), without a Restate server: each attempt
// has `ctx.db` over the object's state in memory, and records the sends it
// makes. A crash is an attempt whose later writes and
// sends are lost: what it stored before stays.
import * as restate from 'restate-session-objects';
import { open } from 'restate-session-objects/internal';
import { describe, expect, it } from 'vitest';
import { memoryStore, sqlAttempt } from './support.ts';

type Send = restate.GenericSend<Uint8Array>;

/** The database's head: the commit point of the flush whose record holds the outbox. */
const HEAD = 'v22/head';
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
      const record = await db.record('outbox');
      await db.close();
      if (record.length === 0) return [];
      const outbox = JSON.parse(new TextDecoder().decode(record)) as { id: string; sends: unknown[] }[];
      return outbox.flatMap(({ id, sends }) => sends.map((_, i) => `transaction/${id}/${i}`));
    },
  };
}

const decode = (s: Send) => JSON.parse(new TextDecoder().decode(s.parameter)) as unknown;

/** A transfer with two sends, as one transaction. */
const transfer = (a: ReturnType<ReturnType<typeof object>['attempt']>) =>
  a.db.transaction(async (tx) => {
    const before = a.sent.length;
    await tx.execute('CREATE TABLE IF NOT EXISTS balance (v INTEGER)');
    await tx.execute('INSERT INTO balance VALUES (5)');
    tx.send(store, 'a').reserve('book');
    tx.send(store, 'b').deposit(5);
    // Nothing is sent before the commit.
    expect(a.sent).toHaveLength(before);
  });

const balance = async (o: ReturnType<typeof object>) => {
  const a = o.attempt();
  const [[count]] = (await a.end(() => a.db.values("SELECT count(*) FROM sqlite_master WHERE name = 'balance'"))) as [
    [number],
  ];
  if (!count) return 0;
  const b = o.attempt();
  return (await b.end(() => b.db.values('SELECT count(*) FROM balance')))[0]![0];
};

describe('the sends of a transaction', () => {
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
    expect(await b.end(() => b.db.values("SELECT name FROM sqlite_master WHERE name LIKE '_restate%'"))).toEqual([]);
  });

  it('sends nothing for a transaction rolled back', async () => {
    const o = object();
    const a = o.attempt();
    const failed = await a
      .end(() =>
        a.db.transaction(async (tx) => {
          await tx.execute('CREATE TABLE t (v INTEGER)');
          tx.send(store, 'a').reserve('book');
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
      const refused = await a.db
        .transaction(async (tx) => {
          await tx.execute('INSERT INTO c VALUES (7)');
          tx.send(store, 'a').reserve('book');
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
    await next.end(() => next.db.values('SELECT 1'));
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
    const decide = crashed.db.transaction(async (tx) => {
      const [[v]] = (await tx.values('SELECT v FROM t')) as [[number]];
      await tx.execute('UPDATE t SET v = 2 WHERE v = 99');
      if (v === 1) tx.send(store, 'a').reserve('book');
    });
    await expect(decide).rejects.toThrow('crashed');
    crashed.abort();
    const lost = await o.outbox();
    expect(lost).toHaveLength(1);
    const next = o.attempt();
    await next.end(() => next.db.values('SELECT 1'));
    expect(next.sent.map((s) => s.idempotencyKey)).toEqual(lost);
    expect(await o.outbox()).toEqual([]);
  });

  it('drops the sends of a commit that a crash lost', async () => {
    // The head that commits the sends is not stored, nor anything after.
    const o = object();
    const crashed = o.attempt();
    const set = o.map.set.bind(o.map);
    o.map.set = (key, value) => {
      if (key === HEAD) crashed.fence.closed = true;
      else set(key, value);
      return o.map;
    };
    await crashed.db
      .transaction(async (tx) => {
        await tx.execute('CREATE TABLE t (v INTEGER)');
        tx.send(store, 'a').reserve('book');
      })
      .catch(() => {});
    crashed.abort();
    o.map.set = set;
    expect(await o.outbox()).toEqual([]);
    const next = o.attempt();
    expect(await next.end(() => next.db.values("SELECT count(*) FROM sqlite_master WHERE name = 't'"))).toEqual([[0]]);
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

  it('writes one head for a transaction without sends, after its tail entry', async () => {
    const o = object();
    const a = o.attempt();
    const sets: string[] = [];
    const set = o.map.set.bind(o.map);
    o.map.set = (key, value) => (sets.push(key), set(key, value));
    await a.end(() => a.db.transaction((tx) => tx.execute('CREATE TABLE t (v INTEGER)')));
    expect(sets.filter((key) => key === HEAD)).toEqual([HEAD]);
    expect(sets.at(-1)).toBe(HEAD);
  });

  it('empties the outbox as soon as the sends are made, before the handler ends', async () => {
    const o = object();
    const a = o.attempt();
    // A suspension after the transaction: the attempt ends there, and its end stores nothing.
    await transfer(a);
    a.abort();
    // Made, then emptied by a commit after them: an attempt that resumes the
    // long wait makes them no more.
    expect(a.sent).toHaveLength(2);
    expect(await o.outbox()).toEqual([]);
    const next = o.attempt();
    await next.end(() => next.db.values('SELECT 1'));
    expect(next.sent).toEqual([]);
  });

  it("keeps a send's delay and its own idempotency key", async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() =>
      a.db.transaction(async (tx) => {
        tx.send(store, 'a', { delay: { seconds: 2 }, idempotencyKey: 'mine' }).reserve('book');
      }),
    );
    expect(a.sent.map((s) => [s.delay, s.idempotencyKey])).toEqual([[2000, 'mine']]);
  });

  it('refuses a send after the body', async () => {
    const o = object();
    const a = o.attempt();
    let late: restate.Transaction | undefined;
    await a.end(() =>
      a.db.transaction(async (tx) => {
        late = tx;
        tx.send(store, 'a').reserve('book');
      }),
    );
    expect(() => late!.send(store, 'a').reserve('pen')).toThrow(restate.TerminalError);
    expect(a.sent.map(decode)).toEqual(['book']);
  });
});

// `ctx.transaction`'s sends, the SDK's outbox, without a Restate server: each
// attempt has the SDK's `ctx.db` and `ctx.transaction` over the object's state
// in memory, and records the sends it makes. A crash is an attempt that never
// ends: what it stored stays, and nothing it would have done after.
import * as restate from '@restatedev/restate-sdk';
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from '../testing.ts';

type Send = restate.GenericSend<Uint8Array>;

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
    attempt() {
      const sent: Send[] = [];
      return { ...sqlAttempt({ map, send: (s) => sent.push(s) }), sent };
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

describe('the outbox of ctx.transaction', () => {
  it('sends once the transaction commits, each with a key of its row', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() => transfer(a));
    expect(a.sent.map((s) => [s.service, s.key, s.method, decode(s)])).toEqual([
      ['Store', 'a', 'reserve', 'book'],
      ['Store', 'b', 'deposit', 5],
    ]);
    const [first, second] = a.sent.map((s) => s.idempotencyKey);
    expect(first).toMatch(/^outbox\/[0-9a-f-]{36}\/1$/);
    expect(second).toBe(first!.replace(/1$/, '2'));
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
    // Nor does a later attempt find anything to send.
    const b = o.attempt();
    await b.end(() => b.db.query('SELECT 1'));
    expect(b.sent).toEqual([]);
  });

  it('sends again, with the same keys, what a crash after the commit may have lost', async () => {
    const o = object();
    const crashed = o.attempt();
    await transfer(crashed); // committed, and sent; the handler never ends
    const lost = crashed.sent.map((s) => s.idempotencyKey);
    crashed.abort();
    // The next attempt's first use of the database sends them again: the
    // target drops a send whose key it has seen.
    const next = o.attempt();
    await next.end(() => next.db.query('SELECT 1'));
    expect(next.sent.map((s) => s.idempotencyKey)).toEqual(lost);
    expect(next.sent.map(decode)).toEqual(['book', 5]);
    // Then they are gone.
    const after = o.attempt();
    expect(await after.end(() => after.db.query('SELECT count(*) FROM _restate_outbox'))).toEqual([[0]]);
    expect(after.sent).toEqual([]);
  });

  it('keys sends by the database id that committed, after a first transaction rolled back', async () => {
    const o = object();
    const crashed = o.attempt();
    // The first transaction with a send makes the outbox, and rolls back.
    await crashed
      .transaction(async ({ tx }) => {
        tx.objectSendClient(store, 'a').reserve('book');
        throw new Error('no');
      })
      .catch(() => {});
    await transfer(crashed);
    crashed.abort();
    const next = o.attempt();
    await next.end(() => next.db.query('SELECT 1'));
    expect(next.sent.map((s) => s.idempotencyKey)).toEqual(crashed.sent.map((s) => s.idempotencyKey));
  });

  it('keeps the rows until the handler ends, past later transactions', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(async () => {
      await transfer(a);
      await transfer(a);
      expect(await a.db.query('SELECT count(*) FROM _restate_outbox')).toEqual([[4]]);
    });
    expect(a.sent).toHaveLength(4);
    const b = o.attempt();
    expect(await b.end(() => b.db.query('SELECT count(*) FROM _restate_outbox'))).toEqual([[0]]);
  });

  it('gives a new database at the same key new keys', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(() => transfer(a));
    o.map.clear(); // ctx.clearAll: the database is gone
    const b = o.attempt();
    await b.end(() => transfer(b));
    expect(b.sent[0]!.idempotencyKey).toMatch(/\/1$/);
    expect(b.sent[0]!.idempotencyKey).not.toBe(a.sent[0]!.idempotencyKey);
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

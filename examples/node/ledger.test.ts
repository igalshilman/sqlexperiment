// The ledger's handlers without a Restate server, with a fake `ctx` whose
// `ctx.sql` runs its body as one transaction of a sqlite-kv database in memory.
import * as restate from '@restatedev/restate-sdk';
import { open } from 'sqlite-kv';
import { describe, expect, it } from 'vitest';
import { memoryStore } from '../testing.ts';
import { handlers, type Transfer } from './ledger.ts';

let n = 0;
async function books() {
  const db = await open(`ledger-${process.pid}-${n++}`, memoryStore());
  const ctx = { sql: <T>(body: (db: restate.SqlDatabase) => Promise<T>) => db.transaction(body) };
  const call = <A, R>(handler: (ctx: restate.SqlObjectContext, arg: A) => Promise<R>, arg: A) =>
    handler(ctx as unknown as restate.SqlObjectContext, arg);
  await call(handlers.init, [4, 100] as [number, number]);
  return {
    transfer: (t: Transfer) => call(handlers.transfer, t),
    audit: () => call(handlers.audit, undefined),
  };
}

describe('the ledger', () => {
  it('moves money once per transfer id', async () => {
    const b = await books();
    const t1 = { id: 't1', from: 0, to: 1, amount: 5 };
    expect(await b.transfer(t1)).toEqual(t1);
    expect(await b.transfer(t1)).toEqual(t1);
    expect(await b.audit()).toEqual({ accounts: 4, total: 400, transfers: 1, inconsistent: 0 });
  });

  it('returns the transfer as applied when its id comes again with other details', async () => {
    const b = await books();
    await b.transfer({ id: 't1', from: 0, to: 1, amount: 5 });
    // A retry that names the earlier call with another amount moves nothing,
    // and says what moved.
    expect(await b.transfer({ id: 't1', from: 2, to: 3, amount: 50 })).toEqual({ id: 't1', from: 0, to: 1, amount: 5 });
    expect(await b.audit()).toEqual({ accounts: 4, total: 400, transfers: 1, inconsistent: 0 });
  });

  it('refuses a transfer that would create or destroy money', async () => {
    const b = await books();
    for (const t of [
      { id: 'to-nowhere', from: 0, to: 99, amount: 5 },
      { id: 'from-nowhere', from: 99, to: 0, amount: 5 },
      { id: 'negative', from: 0, to: 1, amount: -5 },
      { id: 'zero', from: 0, to: 1, amount: 0 },
      { id: 'fraction', from: 0, to: 1, amount: 0.5 },
      { id: 'to-itself', from: 2, to: 2, amount: 5 },
    ]) {
      const refused = await b.transfer(t).then(
        () => `${t.id} was applied`,
        (error: unknown) => error,
      );
      expect(refused).toBeInstanceOf(restate.TerminalError);
    }
    expect(await b.audit()).toEqual({ accounts: 4, total: 400, transfers: 0, inconsistent: 0 });
  });
});

/**
 * 06 · Running twice, changing once
 *
 * When a handler fails before it ends (a crash, a lost connection), Restate
 * retries it: the handler runs again from the start, against the database as
 * stored, so what the failed attempt committed is there. A handler that
 * records its request's id with its writes finds it the second time, and
 * changes nothing.
 *
 *   node src/06_retries.ts                                                          # and register it (see 01)
 *   curl localhost:8080/savings/alice/deposit --json '{"id": "d1", "amount": 10}'   # 10, though its first attempt failed (this process logs it)
 *   curl localhost:8080/savings/alice/deposit --json '{"id": "d1", "amount": 10}'   # 10: the same deposit, sent again
 *   curl localhost:8080/savings/alice/deposit --json '{"id": "d2", "amount": 5}'    # 15
 */
import * as restate from 'restate-sessions';

const failed = new Set<string>();

/** Fail the first attempt of each invocation, as a crash would: Restate retries it. */
function failFirstAttempt(ctx: restate.SessionContext) {
  if (failed.has(ctx.request.id)) return;
  failed.add(ctx.request.id);
  throw new Error('A crash, after the deposit committed');
}

const savings = restate.sessionObject({
  name: 'savings',
  schema: ['CREATE TABLE deposits (id TEXT PRIMARY KEY, amount INTEGER NOT NULL)'],
  handlers: {
    deposit: async (ctx, { id, amount }: { id: string; amount: number }) => {
      // The second time, the deposit's id is there already: OR IGNORE skips it.
      await ctx.db.execute('INSERT OR IGNORE INTO deposits (id, amount) VALUES (?, ?)', [id, amount]);
      failFirstAttempt(ctx);
      const deposits = await ctx.db.get<{ total: number }>('SELECT sum(amount) AS total FROM deposits');
      return deposits?.total;
    },
  },
});

await restate.serve({ services: [savings], port: 9080 });

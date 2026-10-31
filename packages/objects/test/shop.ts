// A shop: a Warehouse object per SKU and an Orders object per order, each with
// its own SQL database, talking through RPC.
//
// Handlers are not replayed: a retried attempt runs again from the start. Two
// things make that safe:
// - Each handler dedupes on a stable id it is given (the order id, a restock
//   id), so running it twice changes nothing the second time.
// - Each call has an idempotency key made from the request id
//   (`ctx.request().id`, the same on every attempt of an invocation) and
//   what the call is for. So a retried attempt reaches the invocation the
//   first one started (and gets its result) instead of starting another.
//   Without a key, each attempt's call would be a new one.
// - The `expire` message is a send of the transaction that stores the order
//   (`tx` of `ctx.transaction`): it is made once the order commits, exactly
//   once, so it needs no key of its own.
// - A call is made outside any transaction: an `Orders` handler reads the
//   order, calls the warehouse, then records the outcome. Only the `db` and
//   `tx` of `ctx.transaction` are part of a transaction; a call made inside
//   its body would be made whether or not it commits, and would hold the
//   transaction open while it waits. The handler is exclusive, so nothing
//   changes the order between the read and the write.
//
// Environment: PORT (default 9080), SHOP_EXPIRY_MS (how long an unpaid order
// holds its stock, default 5000) and ABORT_PER_MILLE (see faults.ts).
import * as restate from 'restate-objects';
import { serveIfMain } from './faults.ts';
import { row } from './sql.ts';

const EXPIRY_MS = Number(process.env.SHOP_EXPIRY_MS ?? 5000);

type Ctx = restate.ObjectContext;

/** A reservation: the order, its quantity, and its state (held, sold, released or rejected). */
export type Reservation = [order: string, qty: number, state: string];

const WAREHOUSE = `
  CREATE TABLE IF NOT EXISTS stock(id INTEGER PRIMARY KEY CHECK (id = 0), available INTEGER NOT NULL);
  INSERT OR IGNORE INTO stock VALUES(0, 0);
  CREATE TABLE IF NOT EXISTS restocks(id TEXT PRIMARY KEY, qty INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS reservations(orderId TEXT PRIMARY KEY, qty INTEGER NOT NULL, state TEXT NOT NULL);`;

/** The stock of one SKU (the object key). */
// Retried soon, as the ledger is: the crash test aborts attempts often, and
// the server's default backoff would grow past its deadline; and without
// end, as an invocation the server pauses after its default attempts (70)
// would wait for a person.
const OPTIONS = {
  retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
};

export const warehouse = restate.object({
  name: 'Warehouse',
  handlers: {
    /** Add `qty` units, once per restock `id`. */
    restock: (ctx: Ctx, { id, qty }: { id: string; qty: number }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(WAREHOUSE);
        const [done] = await row<[number]>(db, 'SELECT count(*) FROM restocks WHERE id = ?', [id]);
        if (done) return false;
        await db.execute('INSERT INTO restocks VALUES(?, ?)', [id, qty]);
        await db.execute('UPDATE stock SET available = available + ?', [qty]);
        return true;
      }),

    /** Hold `qty` units for `order`; false if there are not enough. */
    reserve: (ctx: Ctx, { order, qty }: { order: string; qty: number }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(WAREHOUSE);
        const [held] = await db.values('SELECT state FROM reservations WHERE orderId = ?', [order]);
        if (held) return held[0] !== 'rejected';
        const [available] = await row<[number]>(db, 'SELECT available FROM stock');
        const ok = available >= qty;
        if (ok) await db.execute('UPDATE stock SET available = available - ?', [qty]);
        await db.execute('INSERT INTO reservations VALUES(?, ?, ?)', [order, qty, ok ? 'held' : 'rejected']);
        return ok;
      }),

    /** The held units of `order` are sold. */
    commit: (ctx: Ctx, { order }: { order: string }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(WAREHOUSE);
        await db.execute(`UPDATE reservations SET state = 'sold' WHERE orderId = ? AND state = 'held'`, [order]);
      }),

    /** The held units of `order` go back to stock. */
    release: (ctx: Ctx, { order }: { order: string }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(WAREHOUSE);
        const [held] = await db.values(`SELECT qty FROM reservations WHERE orderId = ? AND state = 'held'`, [order]);
        if (!held) return;
        await db.execute('UPDATE stock SET available = available + ?', [held[0]!]);
        await db.execute(`UPDATE reservations SET state = 'released' WHERE orderId = ?`, [order]);
      }),

    /** Stock and reservations, for audits. */
    report: (ctx: Ctx) =>
      ctx.transaction(async ({ db }) => {
        await db.execute(WAREHOUSE);
        const [available] = await row<[number]>(db, 'SELECT available FROM stock');
        const [restocked] = await row<[number]>(db, 'SELECT coalesce(sum(qty), 0) FROM restocks');
        const reservations = await db.values('SELECT orderId, qty, state FROM reservations ORDER BY orderId');
        return { available, restocked, reservations: reservations as Reservation[] };
      }),
  },
  options: OPTIONS,
});

const ORDER = `CREATE TABLE IF NOT EXISTS orders(id INTEGER PRIMARY KEY CHECK (id = 0), sku TEXT, qty INTEGER, status TEXT)`;

/** An order: its SKU, quantity and status (reserved, rejected, paid or expired). */
export interface Order {
  sku: string;
  qty: number;
  status: string;
}

async function readOrder(db: restate.SqlDatabase): Promise<Order | null> {
  await db.execute(ORDER);
  const [stored] = await db.values('SELECT sku, qty, status FROM orders');
  return stored ? { sku: String(stored[0]), qty: Number(stored[1]), status: String(stored[2]) } : null;
}

/** One order (the object key): placed, then paid, or expired. */
export const orders = restate.object({
  name: 'Orders',
  handlers: {
    /** Reserve the stock; an unpaid order expires after SHOP_EXPIRY_MS. */
    place: async (ctx: Ctx, { sku, qty }: { sku: string; qty: number }) => {
      const placed = await readOrder(ctx.db);
      if (placed) return placed.status;
      const ok = await ctx
        .objectClient(warehouse, sku)
        .reserve({ order: ctx.key, qty }, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:reserve` }));
      const status = ok ? 'reserved' : 'rejected';
      return ctx.transaction(async ({ db, tx }) => {
        await db.execute('INSERT INTO orders VALUES(0, ?, ?, ?)', [sku, qty, status]);
        // Sent with the order's row, once it commits: a crash after the
        // commit cannot lose it, and an order rolled back sends nothing.
        if (ok)
          tx.objectSendClient(orders, ctx.key).expire(restate.rpc.sendOpts({ delay: { milliseconds: EXPIRY_MS } }));
        return status;
      });
    },

    /** Pay a reserved order. */
    pay: async (ctx: Ctx) => {
      const placed = await readOrder(ctx.db);
      if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
      await ctx
        .objectClient(warehouse, placed.sku)
        .commit({ order: ctx.key }, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:commit` }));
      await ctx.db.execute(`UPDATE orders SET status = 'paid'`);
      return 'paid';
    },

    /** Release the stock of an order still unpaid. */
    expire: async (ctx: Ctx) => {
      const placed = await readOrder(ctx.db);
      if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
      await ctx
        .objectClient(warehouse, placed.sku)
        .release({ order: ctx.key }, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:release` }));
      await ctx.db.execute(`UPDATE orders SET status = 'expired'`);
      return 'expired';
    },

    status: (ctx: Ctx) => readOrder(ctx.db),
  },
  options: OPTIONS,
});

serveIfMain(import.meta, [warehouse, orders]);

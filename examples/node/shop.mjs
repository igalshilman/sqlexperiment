// A shop: a Warehouse object per SKU and an Orders object per order, each with
// its own SQL database, talking through RPC.
//
// Handlers are not replayed: a retried attempt runs again from the start. Two
// things make that safe:
// - Each handler dedupes on a stable id it is given (the order id, a restock
//   id), so running it twice changes nothing the second time.
// - Calls and sends get idempotency keys derived from the invocation and the
//   request, so a retried attempt reaches the invocation the first one
//   started (and gets its result) instead of starting another. That covers
//   the delayed `expire` message too.
//
// Environment: PORT (default 9080), SHOP_EXPIRY_MS (how long an unpaid order
// holds its stock, default 5000) and ABORT_PER_MILLE (see faults.mjs).
import * as restate from '@restatedev/restate-sdk';
import { serveIfMain } from './faults.mjs';

const EXPIRY_MS = Number(process.env.SHOP_EXPIRY_MS ?? 5000);

const WAREHOUSE = `
  CREATE TABLE IF NOT EXISTS stock(id INTEGER PRIMARY KEY CHECK (id = 0), available INTEGER NOT NULL);
  INSERT OR IGNORE INTO stock VALUES(0, 0);
  CREATE TABLE IF NOT EXISTS restocks(id TEXT PRIMARY KEY, qty INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS reservations(orderId TEXT PRIMARY KEY, qty INTEGER NOT NULL, state TEXT NOT NULL);`;

/** The stock of one SKU (the object key). */
export const warehouse = restate.object({
  name: 'Warehouse',
  handlers: {
    /** Add `qty` units, once per restock `id`. */
    restock: (ctx, { id, qty }) =>
      ctx.sql(async (db) => {
        await db.execute(WAREHOUSE);
        const [[done]] = await db.query('SELECT count(*) FROM restocks WHERE id = ?', [id]);
        if (done) return false;
        await db.execute('INSERT INTO restocks VALUES(?, ?)', [id, qty]);
        await db.execute('UPDATE stock SET available = available + ?', [qty]);
        return true;
      }),

    /** Hold `qty` units for `order`; false if there are not enough. */
    reserve: (ctx, { order, qty }) =>
      ctx.sql(async (db) => {
        await db.execute(WAREHOUSE);
        const held = await db.query('SELECT state FROM reservations WHERE orderId = ?', [order]);
        if (held.length > 0) return held[0][0] !== 'rejected';
        const [[available]] = await db.query('SELECT available FROM stock');
        const ok = available >= qty;
        if (ok) await db.execute('UPDATE stock SET available = available - ?', [qty]);
        await db.execute('INSERT INTO reservations VALUES(?, ?, ?)', [order, qty, ok ? 'held' : 'rejected']);
        return ok;
      }),

    /** The held units of `order` are sold. */
    commit: (ctx, { order }) =>
      ctx.sql(async (db) => {
        await db.execute(WAREHOUSE);
        await db.execute(`UPDATE reservations SET state = 'sold' WHERE orderId = ? AND state = 'held'`, [order]);
      }),

    /** The held units of `order` go back to stock. */
    release: (ctx, { order }) =>
      ctx.sql(async (db) => {
        await db.execute(WAREHOUSE);
        const held = await db.query(`SELECT qty FROM reservations WHERE orderId = ? AND state = 'held'`, [order]);
        if (held.length === 0) return;
        await db.execute('UPDATE stock SET available = available + ?', [held[0][0]]);
        await db.execute(`UPDATE reservations SET state = 'released' WHERE orderId = ?`, [order]);
      }),

    /** Stock and reservations, for audits. */
    report: (ctx) =>
      ctx.sql(async (db) => {
        await db.execute(WAREHOUSE);
        const [[available]] = await db.query('SELECT available FROM stock');
        const [[restocked]] = await db.query('SELECT coalesce(sum(qty), 0) FROM restocks');
        const reservations = await db.query('SELECT orderId, qty, state FROM reservations ORDER BY orderId');
        return { available, restocked, reservations };
      }),
  },
  options: restate.sqlOptions,
});

const ORDER = `CREATE TABLE IF NOT EXISTS orders(id INTEGER PRIMARY KEY CHECK (id = 0), sku TEXT, qty INTEGER, status TEXT)`;

async function order(db) {
  await db.execute(ORDER);
  const rows = await db.query('SELECT sku, qty, status FROM orders');
  return rows.length === 0 ? null : { sku: rows[0][0], qty: rows[0][1], status: rows[0][2] };
}

/** One order (the object key): placed, then paid, or expired. */
export const orders = restate.object({
  name: 'Orders',
  handlers: {
    /** Reserve the stock; an unpaid order expires after SHOP_EXPIRY_MS. */
    place: (ctx, { sku, qty }) =>
      ctx.sql(async (db) => {
        const placed = await order(db);
        if (placed) return placed.status;
        const ok = await ctx.objectClient(warehouse, sku).reserve({ order: ctx.key, qty });
        const status = ok ? 'reserved' : 'rejected';
        await db.execute('INSERT INTO orders VALUES(0, ?, ?, ?)', [sku, qty, status]);
        if (ok) {
          ctx.objectSendClient(orders, ctx.key).expire(restate.rpc.sendOpts({ delay: { milliseconds: EXPIRY_MS } }));
        }
        return status;
      }),

    /** Pay a reserved order. */
    pay: (ctx) =>
      ctx.sql(async (db) => {
        const placed = await order(db);
        if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
        await ctx.objectClient(warehouse, placed.sku).commit({ order: ctx.key });
        await db.execute(`UPDATE orders SET status = 'paid'`);
        return 'paid';
      }),

    /** Release the stock of an order still unpaid. */
    expire: (ctx) =>
      ctx.sql(async (db) => {
        const placed = await order(db);
        if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
        await ctx.objectClient(warehouse, placed.sku).release({ order: ctx.key });
        await db.execute(`UPDATE orders SET status = 'expired'`);
        return 'expired';
      }),

    status: (ctx) => ctx.sql(order),
  },
  options: restate.sqlOptions,
});

serveIfMain(import.meta, [warehouse, orders]);

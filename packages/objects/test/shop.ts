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
import * as restate from 'restate-session-objects';
import { serveIfMain } from './faults.ts';

const EXPIRY_MS = Number(process.env.SHOP_EXPIRY_MS ?? 5000);

/** A reservation: the order, its quantity, and its state (held, sold, released or rejected). */
export type Reservation = [order: string, qty: number, state: string];

/** The stock of one SKU (the object key). */
// Retried soon, as the ledger is: the crash test aborts attempts often, and
// the server's default backoff would grow past its deadline; and without
// end, as an invocation the server pauses after its default attempts (70)
// would wait for a person.
const OPTIONS = {
  retryPolicy: { initialInterval: 50, maxInterval: 500, maxAttempts: 100_000 },
};

export const warehouse = restate.sessionObject({
  name: 'Warehouse',
  schema: [
    'CREATE TABLE stock (id INTEGER PRIMARY KEY CHECK (id = 0), available INTEGER NOT NULL)',
    'INSERT INTO stock VALUES (0, 0)',
    'CREATE TABLE restocks (id TEXT PRIMARY KEY, qty INTEGER NOT NULL)',
    'CREATE TABLE reservations (orderId TEXT PRIMARY KEY, qty INTEGER NOT NULL, state TEXT NOT NULL)',
  ],
  handlers: {
    /** Add `qty` units, once per restock `id`. */
    restock: (ctx, { id, qty }: { id: string; qty: number }) =>
      ctx.db.transaction(async (tx) => {
        if (await tx.get('SELECT 1 FROM restocks WHERE id = ?', [id])) return false;
        await tx.execute('INSERT INTO restocks VALUES(?, ?)', [id, qty]);
        await tx.execute('UPDATE stock SET available = available + ?', [qty]);
        return true;
      }),

    /** Hold `qty` units for `order`; false if there are not enough. */
    reserve: (ctx, { order, qty }: { order: string; qty: number }) =>
      ctx.db.transaction(async (tx) => {
        const held = await tx.get<{ state: string }>('SELECT state FROM reservations WHERE orderId = ?', [order]);
        if (held) return held.state !== 'rejected';
        const { available } = (await tx.get<{ available: number }>('SELECT available FROM stock'))!;
        const ok = available >= qty;
        if (ok) await tx.execute('UPDATE stock SET available = available - ?', [qty]);
        await tx.execute('INSERT INTO reservations VALUES(?, ?, ?)', [order, qty, ok ? 'held' : 'rejected']);
        return ok;
      }),

    /** The held units of `order` are sold. */
    commit: async (ctx, { order }: { order: string }) => {
      await ctx.db.execute(`UPDATE reservations SET state = 'sold' WHERE orderId = ? AND state = 'held'`, [order]);
    },

    /** The held units of `order` go back to stock. */
    release: (ctx, { order }: { order: string }) =>
      ctx.db.transaction(async (tx) => {
        const held = await tx.get<{ qty: number }>(
          `SELECT qty FROM reservations WHERE orderId = ? AND state = 'held'`,
          [order],
        );
        if (!held) return;
        await tx.execute('UPDATE stock SET available = available + ?', [held.qty]);
        await tx.execute(`UPDATE reservations SET state = 'released' WHERE orderId = ?`, [order]);
      }),
  },
  views: {
    /** Stock and reservations, for audits. */
    report: async (ctx) => {
      const { available } = (await ctx.db.get<{ available: number }>('SELECT available FROM stock'))!;
      const { restocked } = (await ctx.db.get<{ restocked: number }>(
        'SELECT coalesce(sum(qty), 0) AS restocked FROM restocks',
      ))!;
      const reservations = await ctx.db.values('SELECT orderId, qty, state FROM reservations ORDER BY orderId');
      return { available, restocked, reservations: reservations as Reservation[] };
    },
  },
  options: OPTIONS,
});

/** An order: its SKU, quantity and status (reserved, rejected, paid or expired). */
export interface Order {
  sku: string;
  qty: number;
  status: string;
}

const readOrder = async (db: restate.DatabaseReader): Promise<Order | null> =>
  (await db.get<Order>('SELECT sku, qty, status FROM orders')) ?? null;

/** One order (the object key): placed, then paid, or expired. */
export const orders = restate.sessionObject({
  name: 'Orders',
  schema: ['CREATE TABLE orders (id INTEGER PRIMARY KEY CHECK (id = 0), sku TEXT, qty INTEGER, status TEXT)'],
  handlers: {
    /** Reserve the stock; an unpaid order expires after SHOP_EXPIRY_MS. */
    place: async (ctx, { sku, qty }: { sku: string; qty: number }) => {
      const placed = await readOrder(ctx.db);
      if (placed) return placed.status;
      const reserved = await ctx
        .rpc(warehouse, sku, { idempotencyKey: `${ctx.request.id}:reserve` })
        .reserve({ order: ctx.key, qty });
      const status = reserved ? 'reserved' : 'rejected';
      return ctx.db.transaction(async (tx) => {
        await tx.execute('INSERT INTO orders VALUES(0, ?, ?, ?)', [sku, qty, status]);
        // Sent with the order's row, once it commits: a crash after the
        // commit cannot lose it, and an order rolled back sends nothing.
        if (reserved) tx.send(orders, ctx.key, { delay: { milliseconds: EXPIRY_MS } }).expire();
        return status;
      });
    },

    /** Pay a reserved order. */
    pay: async (ctx) => {
      const placed = await readOrder(ctx.db);
      if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
      await ctx.rpc(warehouse, placed.sku, { idempotencyKey: `${ctx.request.id}:commit` }).commit({ order: ctx.key });
      await ctx.db.execute(`UPDATE orders SET status = 'paid'`);
      return 'paid';
    },

    /** Release the stock of an order still unpaid. */
    expire: async (ctx) => {
      const placed = await readOrder(ctx.db);
      if (placed?.status !== 'reserved') return placed?.status ?? 'unknown';
      await ctx.rpc(warehouse, placed.sku, { idempotencyKey: `${ctx.request.id}:release` }).release({ order: ctx.key });
      await ctx.db.execute(`UPDATE orders SET status = 'expired'`);
      return 'expired';
    },
  },
  views: {
    status: async (ctx) => readOrder(ctx.db),
  },
  options: OPTIONS,
});

serveIfMain(import.meta, [warehouse, orders]);

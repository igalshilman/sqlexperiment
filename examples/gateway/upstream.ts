// The outside world the tools reach: two fake APIs, with the limits real
// ones have.
//
// - GET /orders/:id, the order system: slow (400 ms), and it serves at most
//   CAPACITY requests at once per tenant (its API key, `x-tenant`). One more
//   is a 429. It counts the requests of each tenant and each user (`x-user`).
// - POST /refunds, the payment provider: one refund per `idempotency-key`,
//   as Stripe does.
//
//   node gateway/upstream.ts   (on 9300)
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { fileURLToPath } from 'node:url';

/** Requests the order system serves at once, per tenant. */
export const CAPACITY = 2;

/** Order requests of a tenant (`acme`) or a user (`acme/alice`): the most at once, those refused, and when the first and the last arrived. */
export interface Seen {
  peak: number;
  refused: number;
  first: number;
  last: number;
}

/** What the fake APIs saw. */
export interface Stats {
  orders: Record<string, Seen>;
  /** The idempotency key of each refund made. */
  refunds: string[];
}

/** Order `id`: made up, the same every time. */
export const order = (id: string) => {
  const n = id.split('').reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 1009, 7);
  const status = ['shipped', 'delayed at customs', 'waiting for stock', 'delivered'][n % 4]!;
  return { id, status, total: 20 + (n % 80) };
};

/** Serve the fake APIs on `port`. */
export function startUpstream(port: number): { server: Server; stats: Stats; ready: Promise<void> } {
  const stats: Stats = { orders: {}, refunds: [] };
  const inFlight = new Map<string, number>();
  const handle = async (req: IncomingMessage, res: ServerResponse) => {
    const reply = (status: number, value: unknown) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(value));
    };
    for await (const _ of req); // the body, unread
    const id = /^\/orders\/([^/]+)$/.exec(req.url ?? '')?.[1];
    if (req.method === 'GET' && id) {
      const tenant = String(req.headers['x-tenant']);
      const counters = [tenant, `${tenant}/${String(req.headers['x-user'])}`];
      const seen = counters.map((c) => (stats.orders[c] ??= { peak: 0, refused: 0, first: 0, last: 0 }));
      if ((inFlight.get(tenant) ?? 0) >= CAPACITY) {
        for (const s of seen) s.refused++;
        return reply(429, { error: `at most ${CAPACITY} requests at once` });
      }
      counters.forEach((c, i) => {
        inFlight.set(c, (inFlight.get(c) ?? 0) + 1);
        seen[i]!.peak = Math.max(seen[i]!.peak, inFlight.get(c)!);
        seen[i]!.last = Date.now();
        seen[i]!.first ||= seen[i]!.last;
      });
      await new Promise((r) => setTimeout(r, 400));
      for (const c of counters) inFlight.set(c, inFlight.get(c)! - 1);
      return reply(200, order(decodeURIComponent(id)));
    }
    if (req.method === 'POST' && req.url === '/refunds') {
      const key = String(req.headers['idempotency-key']);
      if (!stats.refunds.includes(key)) stats.refunds.push(key);
      return reply(200, { refund: `re_${stats.refunds.indexOf(key)}` });
    }
    reply(404, { error: 'not found' });
  };
  const server = createServer((req, res) => void handle(req, res));
  const ready = new Promise<void>((resolve) => server.listen(port, '127.0.0.1', resolve));
  return { server, stats, ready };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  await startUpstream(9300).ready;
  console.error('fake APIs on http://127.0.0.1:9300');
}

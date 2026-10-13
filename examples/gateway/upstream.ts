// The outside world the tools reach: fake external APIs over HTTP, with the
// limits real ones have.
//
// - GET /orders/:id, the order system: slow (ORDER_LATENCY_MS), and it serves
//   at most CAPACITY requests at once per tenant (its API key, the
//   `x-tenant` header). One more is a 429, as a rate-limited API answers. It
//   also counts the requests of each of a tenant's users (`x-user`).
// - POST /refunds, the payment provider: it refunds once per
//   `idempotency-key`, as Stripe does, and counts the refunds it made.
// - POST /mail, the mail provider, once per `idempotency-key` too.
// - POST /pages, the manager's pager: what the gateway pages about a tool
//   call waiting for approval, with the awakeable to resolve.
// - GET /stats: what each of them saw.
//
//   node gateway/upstream.ts   (UPSTREAM_PORT, default 9300)
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { fileURLToPath } from 'node:url';

/** Requests the order system serves at once, per tenant. */
export const CAPACITY = 2;
export const ORDER_LATENCY_MS = Number(process.env.ORDER_LATENCY_MS ?? 400);

/** A page to the manager: a tool call waiting for approval. */
export interface Page {
  tenant: string;
  tool: string;
  arguments: unknown;
  awakeable: string;
}

/** Order requests served, the most in flight at once, those refused with a 429, and when the first and the last served one arrived. */
export interface Seen {
  served: number;
  peak: number;
  refused: number;
  first: number;
  last: number;
}

/** What the fake APIs saw. */
export interface Stats {
  /** Order requests, per tenant. */
  orders: Record<string, Seen>;
  /** Order requests, per tenant and user (`acme/alice`). */
  users: Record<string, Seen>;
  /** Refunds made, one per idempotency key, and the requests for them. */
  refunds: { made: { key: string; order: string; amount: number }[]; requests: number };
  mails: { sent: number; requests: number };
  pages: Page[];
}

/** The status of order `id`: made up, the same every time. */
export const order = (id: string) => {
  const n = id.split('').reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 1009, 7);
  const status = ['shipped', 'delayed at customs', 'waiting for stock', 'delivered'][n % 4]!;
  return { id, status, customer: `customer-${n % 97}@example.com`, total: 20 + (n % 80) };
};

const body = async (req: IncomingMessage) => {
  let text = '';
  for await (const chunk of req) text += chunk;
  return text ? (JSON.parse(text) as Record<string, unknown>) : {};
};

/** Serve the fake APIs on `port`; `stats` is what they saw so far. */
export function startUpstream(port: number): { server: Server; stats: Stats; ready: Promise<void> } {
  const stats: Stats = {
    orders: {},
    users: {},
    refunds: { made: [], requests: 0 },
    mails: { sent: 0, requests: 0 },
    pages: [],
  };
  const inFlight = new Map<string, number>();
  const mailed = new Set<string>();
  const handle = async (req: IncomingMessage, res: ServerResponse) => {
    const reply = (status: number, value: unknown) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(value));
    };
    const url = new URL(req.url ?? '/', 'http://upstream');
    const key = req.headers['idempotency-key'];
    try {
      const orderPath = /^\/orders\/([^/]+)$/.exec(url.pathname);
      if (req.method === 'GET' && orderPath) {
        const tenant = String(req.headers['x-tenant'] ?? 'anonymous');
        const user = `${tenant}/${String(req.headers['x-user'] ?? 'anonymous')}`;
        const seen = (stats.orders[tenant] ??= { served: 0, peak: 0, refused: 0, first: 0, last: 0 });
        const seenUser = (stats.users[user] ??= { served: 0, peak: 0, refused: 0, first: 0, last: 0 });
        if ((inFlight.get(tenant) ?? 0) >= CAPACITY) {
          seen.refused++;
          seenUser.refused++;
          return reply(429, { error: `at most ${CAPACITY} requests at once` });
        }
        const begin = (s: Seen, counter: string) => {
          const now = (inFlight.get(counter) ?? 0) + 1;
          inFlight.set(counter, now);
          s.peak = Math.max(s.peak, now);
          s.last = Date.now();
          s.first ||= s.last;
        };
        begin(seen, tenant);
        begin(seenUser, user);
        await new Promise((r) => setTimeout(r, ORDER_LATENCY_MS));
        for (const counter of [tenant, user]) inFlight.set(counter, inFlight.get(counter)! - 1);
        seen.served++;
        seenUser.served++;
        return reply(200, order(decodeURIComponent(orderPath[1]!)));
      }
      if (req.method === 'POST' && url.pathname === '/refunds') {
        const { order: id, amount } = (await body(req)) as { order: string; amount: number };
        stats.refunds.requests++;
        if (typeof key !== 'string') return reply(400, { error: 'an idempotency-key is required' });
        let made = stats.refunds.made.find((r) => r.key === key);
        if (!made) stats.refunds.made.push((made = { key, order: id, amount }));
        return reply(200, { refund: `re_${stats.refunds.made.indexOf(made)}`, order: made.order, amount: made.amount });
      }
      if (req.method === 'POST' && url.pathname === '/mail') {
        await body(req);
        stats.mails.requests++;
        if (typeof key === 'string' && !mailed.has(key)) {
          mailed.add(key);
          stats.mails.sent++;
        }
        return reply(200, { queued: true });
      }
      if (req.method === 'POST' && url.pathname === '/pages') {
        const page = (await body(req)) as unknown as Page;
        if (!stats.pages.some((p) => p.awakeable === page.awakeable)) {
          stats.pages.push(page);
          console.error(`[pager] ${page.tenant} asks to run ${page.tool} ${JSON.stringify(page.arguments)}`);
          console.error(
            `[pager]   approve: curl localhost:8080/restate/awakeables/${page.awakeable}/resolve --json '{"approved": true, "by": "manager"}'`,
          );
        }
        return reply(200, { paged: true });
      }
      if (req.method === 'GET' && url.pathname === '/stats') return reply(200, stats);
      reply(404, { error: 'not found' });
    } catch (e) {
      reply(500, { error: String(e) });
    }
  };
  const server = createServer((req, res) => void handle(req, res));
  const ready = new Promise<void>((resolve) => server.listen(port, '127.0.0.1', resolve));
  return { server, stats, ready };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const port = Number(process.env.UPSTREAM_PORT ?? 9300);
  await startUpstream(port).ready;
  console.error(`fake APIs on http://127.0.0.1:${port}`);
}

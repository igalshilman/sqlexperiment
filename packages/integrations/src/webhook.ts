// Webhooks into agents: a Restate service whose one handler, `webhook`,
// takes another system's webhook (Linear's, GitHub's...), checks its
// signature, and delivers it to agents, as a message saying where it comes
// from: to the message an agent answers, if it answers one, or else as a
// message of its own.
//
//   const issues = webhook({
//     name: 'linear',
//     auth: { header: 'linear-signature', secret: process.env.LINEAR_WEBHOOK_SECRET! },
//     idempotencyHeader: 'linear-delivery',
//     route: [triage],
//     key: ({ data }) => data.identifier,
//   });
//   await serve(issues, triage);
//
// The other system posts to Restate's ingress, at `/<name>/webhook`.
import { createHmac, timingSafeEqual } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import { deliver, hasCompanion } from './signals.ts';

/** How a webhook is signed: the HMAC-SHA256 of its body with `secret`, in hex, in the request's `header`. */
export interface WebhookAuth {
  /** The header with the signature, such as `linear-signature` (GitHub's `sha256=` prefix is taken too). */
  header: string;
  /** The webhook's signing secret, which the other system shows when the webhook is made. */
  secret: string;
}

/** An agent (an `object()` or `agent()` of restate-agents, with a `chat` handler) that webhooks go to. */
type Route = restate.VirtualObjectDefinition<string, { chat: unknown }>;

/**
 * A delivery's body: JSON, as the other system sends it, read as it is (it is
 * theirs to shape, not checked here).
 */
// oxlint-disable-next-line typescript/no-explicit-any
export type WebhookEvent = Record<string, any>;

/** A webhook service's options. */
export interface WebhookOptions<P extends string = string> {
  /** The service's name, which its URL starts with: `/<name>/webhook`. */
  name: P;
  /** What the webhook is, for the UI and the admin API. */
  description?: string;
  /** Its signature: a webhook without it is refused (401). Without `auth`, any request is taken. */
  auth?: WebhookAuth;
  /**
   * The header with the delivery's id, which a delivery sent again has too
   * (such as `linear-delivery`): it reaches the same message, not a second
   * one. Without it, each request is a delivery of its own.
   */
  idempotencyHeader?: string;
  /** The agents each delivery goes to. */
  route: Route[];
  /**
   * The key of the agents' object that a delivery goes to, from its body
   * (the issue, say: one conversation per issue), or undefined to drop the
   * delivery. By default, the delivery's id: a conversation per delivery.
   * The event is the body's JSON, untyped: `key: ({ data }) => data.identifier`.
   */
  key?(this: void, event: WebhookEvent): string | undefined;
  /**
   * What of a delivery the agents read, from its body: by default, all of
   * it. Some systems send much more than an agent needs (GitHub sends the
   * whole repository with each event): `message: ({ comment }) => ({ body: comment.body })`.
   */
  message?(this: void, event: WebhookEvent): unknown;
}

/**
 * The message an agent gets for a webhook's delivery: its body, as JSON, in
 * a `<webhook>` element that says which webhook, and which delivery.
 *
 *   <webhook name="linear" delivery="5d2…">
 *   {"action": "create", "type": "Issue", ...}
 *   </webhook>
 */
export const webhookMessage = (name: string, delivery: string, event: unknown) =>
  `<webhook name="${name}" delivery="${delivery}">\n${JSON.stringify(event, null, 2)}\n</webhook>`;

/** Whether `signature` is the HMAC-SHA256 of `body` with `secret`, in hex. */
const signed = (body: Uint8Array, signature: string | undefined, secret: string) => {
  const expected = createHmac('sha256', secret).update(body).digest();
  const given = Buffer.from((signature ?? '').replace(/^sha256=/, ''), 'hex');
  return given.length === expected.length && timingSafeEqual(given, expected);
};

/** `body` as a JSON object, or a terminal error (400) if it is not. */
const parsed = (body: Uint8Array): WebhookEvent => {
  let event: unknown;
  try {
    event = JSON.parse(new TextDecoder().decode(body));
  } catch {
    throw new restate.TerminalError('The webhook is not JSON', { errorCode: 400 });
  }
  if (typeof event !== 'object' || event === null || Array.isArray(event)) {
    throw new restate.TerminalError('The webhook is not a JSON object', { errorCode: 400 });
  }
  return event;
};

/**
 * A webhook into agents: a service `name`, whose `webhook` handler checks a
 * delivery's signature, and delivers it to each agent of `route`, keyed by
 * `key`, as a {@link webhookMessage}, through the agent's `signals`: to the
 * message the agent answers, if any, which takes it at its next step (as
 * `steer`), or else as a message of its own (as `chat`). A turn that ends,
 * or is stopped, before it takes it answers it after. It answers once the
 * deliveries are sent, not answered: a system that waits seconds for an
 * answer (Linear, GitHub) gets one at once. What it is (its routes, auth and
 * idempotency) is in the handler's metadata, which the admin API shows.
 */
export const webhook = <P extends string>({
  name,
  description,
  auth,
  idempotencyHeader,
  route,
  key,
  message,
}: WebhookOptions<P>) => {
  if (route.length === 0) throw new Error(`webhook ${name}: route it to at least one agent`);
  for (const agent of route) {
    if (!hasCompanion(agent)) throw new Error(`webhook ${name}: ${agent.name} is not an object() or agent()`);
  }
  if (auth && !auth.secret) throw new Error(`webhook ${name}: its auth has no secret (unset in the environment?)`);
  const what = description ?? `A webhook into ${route.map((r) => r.name).join(', ')}`;
  return restate.service({
    name,
    description: what,
    handlers: {
      webhook: restate.handlers.handler(
        {
          input: restate.serde.binary,
          accept: '*/*',
          description: what,
          metadata: {
            'restate-agents.kind': 'webhook',
            'restate-agents.routes': route.map((r) => r.name).join(','),
            'restate-agents.auth': auth ? `hmac-sha256:${auth.header}` : 'none',
            ...(idempotencyHeader ? { 'restate-agents.idempotency-header': idempotencyHeader } : {}),
          },
        },
        async (ctx: restate.Context, body: Uint8Array) => {
          const headers = ctx.request().headers;
          if (auth && !signed(body, headers.get(auth.header), auth.secret)) {
            throw new restate.TerminalError(`Not signed: no valid ${auth.header}`, { errorCode: 401 });
          }
          const event = parsed(body);
          const delivery = (idempotencyHeader && headers.get(idempotencyHeader)) || ctx.request().id;
          const to = key ? key(event) : delivery;
          if (to === undefined) return;
          const text = webhookMessage(name, delivery, message ? message(event) : event);
          for (const agent of route) deliver(ctx, agent, to, text, { idempotencyKey: `webhook:${name}:${delivery}` });
        },
      ),
    },
  });
};

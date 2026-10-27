// webhook(): a delivery's signature is checked, and it goes to its agents,
// through their `signals` (`deliver`), once per delivery, as a message that
// says where it comes from. The handler runs on a fake context, which
// records its sends; restate-tools.ts delivers to a running turn, on Restate.
import { createHmac } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import { companionOf, object, webhook, webhookMessage } from 'restate-agents';
import { describe, expect, it } from 'vitest';

const triage = object({ name: 'triage', handlers: { chat: async (_ctx, text: string) => text } });
const audit = object({ name: 'audit', handlers: { chat: async (_ctx, text: string) => text } });

const issues = webhook({
  name: 'linear',
  auth: { header: 'linear-signature', secret: 'shh' },
  idempotencyHeader: 'linear-delivery',
  route: [triage, audit],
  key: ({ action, data }) => (action === 'create' ? data.identifier : undefined),
});

type Handler = (ctx: unknown, body: Uint8Array) => Promise<void>;
const handler = (issues as unknown as { service: { webhook: Handler } }).service.webhook;

/** A delivery of `event`, signed with `secret`: what the handler sent. */
async function deliver(event: unknown, { secret = 'shh', delivery = 'd-1' } = {}) {
  const body = new TextEncoder().encode(JSON.stringify(event));
  const headers = new Map([
    ['linear-signature', createHmac('sha256', secret).update(body).digest('hex')],
    ['linear-delivery', delivery],
  ]);
  const sent: { object: string; key: string; agent: unknown; text: string; idempotencyKey?: string }[] = [];
  const ctx = {
    request: () => ({ id: 'inv_1', headers }),
    objectSendClient: (target: { name: string }, key: string) => ({
      deliver: (input: { agent: unknown; text: string }, opts: restate.Opts<unknown, unknown>) =>
        sent.push({ object: target.name, key, ...input, idempotencyKey: opts.getOpts().idempotencyKey }),
    }),
  };
  await handler(ctx, body);
  return sent;
}

const created = { action: 'create', data: { identifier: 'ENG-1' } };

describe('a webhook', () => {
  it("delivers to each agent of its route, keyed by its body, through the agent's signals", async () => {
    const sent = await deliver(created);
    expect(sent.map((s) => [s.object, s.key, s.agent])).toEqual([
      ['signals', 'triage:ENG-1', { service: 'triage', key: 'ENG-1' }],
      ['signals', 'audit:ENG-1', { service: 'audit', key: 'ENG-1' }],
    ]);
    expect(sent[0]!.text).toBe(webhookMessage('linear', 'd-1', created));
    expect(sent[0]!.text).toMatch(/^<webhook name="linear" delivery="d-1">\n\{/);
  });

  it('makes a delivery sent again reach the same message: its id is the idempotency key', async () => {
    const [first] = await deliver(created);
    const [again] = await deliver(created);
    const [other] = await deliver(created, { delivery: 'd-2' });
    expect(again!.idempotencyKey).toBe(first!.idempotencyKey);
    expect(other!.idempotencyKey).not.toBe(first!.idempotencyKey);
  });

  it('refuses a delivery that is not signed with its secret', async () => {
    const forged = deliver(created, { secret: 'guess' });
    await expect(forged).rejects.toBeInstanceOf(restate.TerminalError);
    await expect(forged).rejects.toMatchObject({ code: 401 });
  });

  it('refuses a delivery that is not a JSON object', async () => {
    await expect(deliver(['ENG-1'])).rejects.toMatchObject({ code: 400 });
  });

  it('drops a delivery its key gives no key for', async () => {
    expect(await deliver({ action: 'update', data: { identifier: 'ENG-1' } })).toEqual([]);
  });

  it('says what it is in its handler metadata', () => {
    const [symbol] = Object.getOwnPropertySymbols(handler);
    const wrapper = (handler as unknown as Record<symbol, { options: { metadata: Record<string, string> } }>)[symbol!]!;
    expect(wrapper.options.metadata).toEqual({
      'restate-agents.kind': 'webhook',
      'restate-agents.routes': 'triage,audit',
      'restate-agents.auth': 'hmac-sha256:linear-signature',
      'restate-agents.idempotency-header': 'linear-delivery',
    });
  });

  it('fails at its definition with a route that is not an agent of restate-agents', () => {
    const plain = restate.object({ name: 'plain', handlers: { chat: async (_ctx: restate.ObjectContext) => '' } });
    expect(() => webhook({ name: 'linear', route: [plain] })).toThrow(/plain is not an object\(\) or agent\(\)/);
  });

  it('fails at its definition with a secret that is not set', () => {
    expect(() =>
      webhook({ name: 'linear', auth: { header: 'linear-signature', secret: '' }, route: [triage] }),
    ).toThrow(/no secret/);
  });
});

describe("an agent's companion", () => {
  it('is found from the agent: its signals object, keyed `<agent>:<key>`', () => {
    const companion = companionOf(triage, 'ENG-1');
    expect(companion.object.name).toBe('signals');
    expect(companion.key).toBe('triage:ENG-1');
    expect(companion.agent).toEqual({ service: 'triage', key: 'ENG-1' });
    expect(() => companionOf({ name: 'plain' }, 'k')).toThrow(/plain is not an agent of restate-agents/);
  });
});

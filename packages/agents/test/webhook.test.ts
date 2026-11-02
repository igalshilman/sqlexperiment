// webhook(): a delivery's signature is checked, and it goes to its agents'
// `chat` (`deliver`), once per delivery, as a message that says where it
// comes from. The handler runs on a fake context, which records its sends;
// restate-tools.ts delivers to an agent on Restate.
import { createHmac } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import { companionOf, deliver as deliverTo, sessionObject, webhook, webhookMessage } from 'restate-agents';
import { served } from 'restate-sessions/internal';
import { describe, expect, it } from 'vitest';

const triage = sessionObject({ name: 'triage', handlers: { chat: async (_ctx, text: string) => text } });
const audit = sessionObject({ name: 'audit', handlers: { chat: async (_ctx, text: string) => text } });

const issues = webhook({
  name: 'linear',
  auth: { header: 'linear-signature', secret: 'shh' },
  idempotencyHeader: 'linear-delivery',
  route: [triage, audit],
  key: ({ action, data }: { action: string; data: { identifier: string } }) =>
    action === 'create' ? data.identifier : undefined,
});

type Handler = (ctx: unknown, body: Uint8Array) => Promise<void>;
const handlerOf = (service: unknown) => (service as { service: { webhook: Handler } }).service.webhook;

/** A delivery of `event`, signed with `secret`, to `to`'s handler: what the handler sent. */
async function deliver(event: unknown, { secret = 'shh', delivery = 'd-1', to = issues } = {}) {
  const body = new TextEncoder().encode(JSON.stringify(event));
  const headers = new Map([
    ['linear-signature', createHmac('sha256', secret).update(body).digest('hex')],
    ['linear-delivery', delivery],
  ]);
  const sent: { object: string; key: string; method: string; text: string; idempotencyKey?: string }[] = [];
  const ctx = {
    request: () => ({ id: 'inv_1', headers }),
    genericSend: (s: { service: string; method: string; key: string; parameter: string; idempotencyKey?: string }) =>
      sent.push({
        object: s.service,
        key: s.key,
        method: s.method,
        text: s.parameter,
        idempotencyKey: s.idempotencyKey,
      }),
  };
  await handlerOf(to)(ctx, body);
  return sent;
}

const created = { action: 'create', data: { identifier: 'ENG-1' } };

describe('a webhook', () => {
  it('takes its headers in any case, as Restate gives them in lower case', async () => {
    const capitalized = webhook({
      name: 'linear',
      auth: { header: 'Linear-Signature', secret: 'shh' },
      idempotencyHeader: 'Linear-Delivery',
      route: [triage],
      key: () => 'ENG-1',
    });
    const [sent] = await deliver(created, { to: capitalized });
    expect(sent!.idempotencyKey).toBe('webhook:linear:d-1');
  });

  it('delivers to each agent of its route, keyed by its body, as a message to its chat', async () => {
    const sent = await deliver(created);
    expect(sent.map((s) => [s.object, s.key, s.method])).toEqual([
      ['triage', 'ENG-1', 'chat'],
      ['audit', 'ENG-1', 'chat'],
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

  it('delivers what `message` takes of the body, if it has one', async () => {
    const trimmed = webhook({
      name: 'linear',
      route: [triage],
      key: ({ data }) => data.identifier,
      message: ({ action }) => ({ action }),
    });
    const run = (trimmed as unknown as { service: { webhook: Handler } }).service.webhook;
    const sent: string[] = [];
    const ctx = {
      request: () => ({ id: 'inv_1', headers: new Map() }),
      genericSend: ({ parameter }: { parameter: string }) => sent.push(parameter),
    };
    await run(ctx, new TextEncoder().encode(JSON.stringify(created)));
    expect(sent).toEqual([webhookMessage('linear', 'inv_1', { action: 'create' })]);
  });

  it('drops a delivery its key gives no key for', async () => {
    expect(await deliver({ action: 'update', data: { identifier: 'ENG-1' } })).toEqual([]);
  });

  it('says what it is in its handler metadata', () => {
    const handler = handlerOf(issues);
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
    expect(() => webhook({ name: 'linear', route: [plain] })).toThrow(/plain is not a sessionObject\(\) or agent\(\)/);
  });

  it('fails at its definition with a secret that is not set', () => {
    expect(() =>
      webhook({ name: 'linear', auth: { header: 'linear-signature', secret: '' }, route: [triage] }),
    ).toThrow(/no secret/);
  });
});

describe("an agent's companion", () => {
  it('is served beside the agent: restate.serve of an agent serves its signals too', () => {
    expect(served([triage, audit]).map((s) => s.name)).toEqual(['triage', 'signals', 'audit']);
  });

  it('is found from the agent: its signals object, keyed `<agent>:<key>`', () => {
    const companion = companionOf(triage, 'ENG-1');
    expect(companion.object.name).toBe('signals');
    expect(companion.key).toBe('triage:ENG-1');
    expect(companion.agent).toEqual({ service: 'triage', key: 'ENG-1' });
    expect(() => companionOf({ name: 'plain' }, 'k')).toThrow(/plain is not an agent of restate-agents/);
  });

  it("takes a delivery from a session object's handler, as from one of the SDK's", () => {
    const sent: unknown[] = [];
    const ctx = { send: (message: unknown) => sent.push(message) };
    deliverTo(ctx, triage, 'ENG-1', 'closed', { idempotencyKey: 'd-1' });
    expect(sent).toEqual([
      { service: 'triage', handler: 'chat', key: 'ENG-1', input: 'closed', idempotencyKey: 'd-1' },
    ]);
  });
});

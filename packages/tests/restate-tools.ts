// Restate's own tools (restate-agents' call, signal and workflow) against a
// real Restate server: the support agent of examples/intermediate/support.ts on a scripted
// model, which looks the order up, waits for approval, then refunds.
//
// Checks: the agent waits for the signal (`pending` lists the wait, with
// what the model waits for; `status` and `messages` answer meanwhile) until
// it is resolved by its id, then answers
// with every tool's result; a signal no one waits for is dropped, unless it
// is kept; a cancelled wait leaves `pending`; `stop` stops a message being
// answered, and `steer` adds to it; `follow` gives the conversation as it
// changes; a message may be plain text; a webhook's delivery goes to the
// message being answered, once, or else is a message of its own, and is
// answered after a turn that is stopped; `clear` forgets a conversation,
// stopping its turn;
// and a handler's terminal error is the tool's answer.
//
//   pnpm --filter tests tools   (RESTATE_SERVER=/path/to/restate-server to use another)
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as restate from '@restatedev/restate-sdk';
import { createModels } from '@earendil-works/pi-ai/models';
import { call, signal, signals, webhook, workflow, type Messages, type Status, type Waiting } from 'restate-agents';
import * as clients from '@restatedev/restate-sdk-clients';
import { agent, type ChatMessage, follow, type Followed } from 'restate-agents/client';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';
import { callTools, fakeProvider, say, textOf } from './fakes/pi.ts';
import { RESTATE_SERVER as SERVER } from './restate-server.ts';

const INGRESS = 'http://127.0.0.1:8080';
const ADMIN = 'http://127.0.0.1:9070';
const PORT = 9301;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const orders = restate.service({
  name: 'orders',
  handlers: {
    status: async (_ctx: restate.Context, { order }: { order: string }) => {
      if (order !== '103') throw new restate.TerminalError(`no order ${order}`);
      return `order ${order}: delivered`;
    },
  },
});
const refund = restate.workflow({
  name: 'refund',
  handlers: {
    run: async (ctx: restate.WorkflowContext, { order }: { order: string }) => {
      await ctx.sleep({ milliseconds: 200 });
      return `refunded ${order}`;
    },
  },
});

// The model: look the order up, wait for approval, refund, then report.
const models = createModels();
models.setProvider(
  fakeProvider('script', 'script', ({ messages }) => {
    const results = messages.filter((m) => m.role === 'toolResult').map((m) => textOf(m.content));
    const asked = messages.find((m) => m.role === 'user');
    const order = /\d+/.exec(asked ? textOf(asked.content) : '')?.[0] ?? '';
    if (results.length === 0) return callTools([['orders_status', { order }]]);
    if (results[0]!.startsWith('Error')) return say(results[0]!);
    if (results.length === 1) return callTools([['wait_for_approval', { order }]]);
    if (results.length === 2) return callTools([['refund', { order: '103' }]]);
    return say(results.join(' / '));
  }),
);

const support = pi.agent({
  name: 'support',
  model: 'script/script',
  models,
  tools: [
    call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) }),
    signal('approval', { description: 'Wait for a manager', input: z.object({ order: z.string() }) }),
    workflow(refund, { description: 'Refund', input: z.object({ order: z.string() }) }),
  ],
});

// A workflow that calls the agent like any handler, as examples/intermediate/triage.ts does.
const triage = restate.workflow({
  name: 'triage',
  handlers: {
    run: async (ctx: restate.WorkflowContext, ticket: string) => ctx.objectClient(support, ctx.key).chat(ticket),
  },
});

// Deliveries to the support agent, keyed by the customer they are about.
const hooks = webhook({
  name: 'hooks',
  idempotencyHeader: 'x-delivery',
  route: [support],
  key: ({ customer }: { customer: string }) => customer,
});

/** `messages` as lines: each message's role, then its parts. */
const lines = (messages: Messages['messages']) =>
  messages.map((m) => [
    m.role,
    ...m.parts.map((p) => (p.type === 'text' ? p.text : `${p.name}(${JSON.stringify(p.input)}): ${p.output}`)),
  ]);

const dir = mkdtempSync(join(tmpdir(), 'restate-tools-'));
const server = spawn(SERVER, ['--base-dir', dir], {
  env: { ...process.env, RESTATE_BIND_IP: '127.0.0.1', RESTATE_LISTEN_MODE: 'tcp' },
  stdio: 'ignore',
});
try {
  await restate.serve({ services: [orders, refund, support, triage, hooks, signals], port: PORT });
  for (
    let i = 0;
    !(await fetch(`${ADMIN}/health`).then(
      (r) => r.ok,
      () => false,
    ));
    i++
  ) {
    if (i > 300) throw new Error('restate-server did not start');
    await sleep(100);
  }
  const registered = await fetch(`${ADMIN}/deployments`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ uri: `http://127.0.0.1:${PORT}` }),
  });
  assert.ok(registered.ok, await registered.text());

  const post = async (path: string, body?: unknown) => {
    const response = await fetch(`${INGRESS}/${path}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body ?? null),
    });
    const text = await response.text();
    assert.ok(response.ok, `${path}: ${response.status} ${text}`);
    return text ? (JSON.parse(text) as unknown) : undefined;
  };

  /** The waits of `object` (such as `support/alice`), once there is one. */
  const waits = async (object: string) => {
    for (let i = 0; ; i++) {
      const pending = (await post(`${object}/pending`)) as Waiting[];
      if (pending.length > 0) return pending;
      if (i > 300) throw new Error(`${object} does not wait for the approval`);
      await sleep(100);
    }
  };

  // The agent waits for the approval of what it says, and goes on once it comes.
  const answer = post('support/alice/chat', 'Refund order 103, please');
  const [wait] = await waits('support/alice');
  assert.equal(wait!.name, 'approval');
  assert.deepEqual(wait!.input, { order: '103' });
  // While it waits, what it is doing, and the conversation so far, at once.
  const status = (await post('support/alice/status')) as Status;
  assert.ok(status.answering?.invocationId.startsWith('inv_'), JSON.stringify(status));
  assert.deepEqual(status.waiting, [wait]);
  const sofar = (await post('support/alice/messages')) as Messages;
  assert.deepEqual(lines(sofar.messages), [
    ['user', 'Refund order 103, please'],
    ['assistant', 'orders_status({"order":"103"}): order 103: delivered'],
    ['assistant', 'wait_for_approval({"order":"103"}): undefined'],
  ]);
  assert.deepEqual(await post('support/alice/signal', { id: wait!.id, value: 'approved by Kim' }), {
    resolved: wait!.id,
  });
  assert.equal(await answer, 'order 103: delivered / approved by Kim / refunded 103');
  assert.deepEqual(await post('support/alice/pending'), []);
  assert.deepEqual(await post('support/alice/status'), { answering: null, waiting: [] });
  // The conversation: what was said, with the tool calls, as plain `curl` gets it.
  const said = (await (await fetch(`${INGRESS}/support/alice/messages`, { method: 'POST' })).json()) as Messages;
  assert.deepEqual(lines(said.messages), [
    ['user', 'Refund order 103, please'],
    ['assistant', 'orders_status({"order":"103"}): order 103: delivered'],
    ['assistant', 'wait_for_approval({"order":"103"}): approved by Kim'],
    ['assistant', 'refund({"order":"103"}): refunded 103'],
    ['assistant', 'order 103: delivered / approved by Kim / refunded 103'],
  ]);
  assert.equal(said.more, false);
  // A page at a time: the latest two, then the one before them.
  const latest = (await post('support/alice/messages', { limit: 2 })) as Messages;
  assert.deepEqual([latest.messages.map((m) => m.id), latest.more], [[3, 4], true]);
  const older = (await post('support/alice/messages', { limit: 1, before: 3 })) as Messages;
  assert.deepEqual(older.messages, [said.messages[2]]);

  // A signal no one waits for is dropped: it approves nothing later.
  assert.deepEqual(await post('support/bob/signal', { name: 'approval', value: 'stale' }), { resolved: null });
  const later = post('support/bob/chat', 'Refund order 103');
  await waits('support/bob');
  await post('support/bob/signal', { name: 'approval', value: 'approved now' });
  assert.equal(await later, 'order 103: delivered / approved now / refunded 103');

  // Unless it is kept, for the next wait for its name.
  await post('support/frank/signal', { name: 'approval', value: 'approved early', keep: true });
  assert.equal(
    await post('support/frank/chat', 'Refund order 103'),
    'order 103: delivered / approved early / refunded 103',
  );

  // A cancelled invocation no longer waits.
  const sent = (await post('support/grace/chat/send', 'Refund order 103')) as { invocationId: string };
  await waits('support/grace');
  const cancelled = await fetch(`${ADMIN}/invocations/${sent.invocationId}/cancel`, { method: 'PATCH' });
  assert.ok(cancelled.ok, await cancelled.text());
  for (let i = 0; ((await post('support/grace/pending')) as Waiting[]).length > 0; i++) {
    if (i > 300) throw new Error('a cancelled wait is still pending');
    await sleep(100);
  }
  // The cancelled turn is over: it ends cancelled, the agent does not go on
  // (no refund), and the wait's tool call ends with an error.
  const attached = await fetch(`${INGRESS}/restate/invocation/${sent.invocationId}/attach`);
  assert.equal(attached.status, 409, await attached.text());
  assert.deepEqual(await post('support/grace/status'), { answering: null, waiting: [] });
  const graces = lines(((await post('support/grace/messages')) as Messages).messages);
  assert.deepEqual(graces.slice(0, 2), [
    ['user', 'Refund order 103'],
    ['assistant', 'orders_status({"order":"103"}): order 103: delivered'],
  ]);
  assert.match(String(graces[2]?.[1]), /^wait_for_approval\(\{"order":"103"\}\): .*aborted/s);
  assert.equal(graces.length, 3, JSON.stringify(graces));

  // A message as plain text, as `curl -d` sends it.
  await post('support/carol/signal', { name: 'approval', value: 'approved', keep: true });
  const plain = await fetch(`${INGRESS}/support/carol/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: 'Refund order 103',
  });
  assert.equal(await plain.json(), 'order 103: delivered / approved / refunded 103');

  // From a workflow: the agent's answer is the workflow's result.
  await post('support/erin/signal', { name: 'approval', value: 'approved', keep: true });
  assert.equal(await post('triage/erin/run', 'Refund order 103'), 'order 103: delivered / approved / refunded 103');

  // Sent without waiting, and its answer fetched later: by the invocation's
  // id, or by the idempotency key it was sent with.
  // Through Restate's client, with the agent's types of restate-agents/client.
  const ingress = clients.connect({ url: INGRESS });
  const henry = ingress.objectClient(agent('support'), 'henry');
  assert.deepEqual(await henry.signal({ name: 'approval', value: 'approved', keep: true }), { resolved: null });
  const henrys = await ingress
    .objectSendClient(agent('support'), 'henry')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-henry' }));
  assert.equal(await ingress.result(henrys), 'order 103: delivered / approved / refunded 103');
  assert.deepEqual(await henry.status(), { answering: null, waiting: [] });
  assert.equal((await henry.messages({ limit: 1 })).messages[0]?.parts[0]?.type, 'text');

  // `stop` stops the message being answered, through the ingress: the next
  // message is answered afresh.
  const jack = ingress.objectClient(agent('support'), 'jack');
  const jacks = await ingress
    .objectSendClient(agent('support'), 'jack')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-jack' }));
  await waits('support/jack');
  assert.deepEqual(await jack.stop(), { stopped: jacks.invocationId });
  await assert.rejects(ingress.result(jacks), /409/);
  await jack.signal({ name: 'approval', value: 'approved', keep: true });
  assert.match(await jack.chat('Refund order 103'), /refunded 103$/);
  assert.deepEqual(await jack.status(), { answering: null, waiting: [] });
  assert.deepEqual(await jack.stop(), { stopped: null });

  // `steer` adds to the message being answered: the agent takes it at its
  // next tool call (here, once the approval came).
  const liam = ingress.objectClient(agent('support'), 'liam');
  const liams = await ingress
    .objectSendClient(agent('support'), 'liam')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-liam' }));
  await waits('support/liam');
  assert.deepEqual(await liam.steer('Quickly, please'), { invocationId: liams.invocationId, steered: true });
  await liam.signal({ name: 'approval', value: 'approved' });
  assert.equal(await ingress.result(liams), 'order 103: delivered / approved / refunded 103');
  assert.deepEqual(lines((await liam.messages()).messages), [
    ['user', 'Refund order 103'],
    ['assistant', 'orders_status({"order":"103"}): order 103: delivered'],
    ['assistant', 'wait_for_approval({"order":"103"}): approved'],
    ['user', 'Quickly, please'],
    ['assistant', 'refund({"order":"103"}): refunded 103'],
    ['assistant', 'order 103: delivered / approved / refunded 103'],
  ]);

  // `clear` forgets a conversation, and its followers see it empty; with a
  // message being answered, it stops it, and the next message starts anew.
  const before = (await henry.notifications()).revision;
  assert.deepEqual(await henry.clear(), { stopped: null });
  assert.deepEqual((await henry.messages()).messages, []);
  assert.ok((await henry.notifications()).revision > before);
  const pats = await ingress
    .objectSendClient(agent('support'), 'pat')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-pat' }));
  await waits('support/pat');
  const pat = ingress.objectClient(agent('support'), 'pat');
  const forgotten = await pat.messages();
  assert.deepEqual(await pat.clear(), { stopped: pats.invocationId });
  await assert.rejects(ingress.result(pats), /409/);
  assert.deepEqual(await pat.status(), { answering: null, waiting: [] });
  assert.deepEqual(await pat.pending(), []);
  assert.deepEqual((await pat.messages()).messages, []);
  await pat.signal({ name: 'approval', value: 'approved', keep: true });
  assert.match(await pat.chat('Refund order 103'), /refunded 103$/);
  assert.equal(lines((await pat.messages()).messages).length, 5);
  // Its ids and cursors go on: a follower reads on, and drops what came before `first`.
  const anew = await pat.messages({ cursor: forgotten.cursor });
  assert.equal(anew.messages.length, 5);
  assert.equal(anew.first, forgotten.messages.at(-1)!.id + 1);
  assert.equal(anew.messages[0]!.id, anew.first);

  /** Post a delivery of `note` about `customer`, as delivery `id`, and wait until the agent's signals have it. */
  const hook = async (customer: string, note: string, id: string) => {
    const response = await fetch(`${INGRESS}/hooks/webhook`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-delivery': id },
      body: JSON.stringify({ customer, note }),
    });
    assert.ok(response.ok, await response.text());
    const delivered = `SELECT status FROM sys_invocation WHERE target = 'signals/support:${customer}/deliver' AND idempotency_key = 'webhook:hooks:${id}'`;
    for (let i = 0; ; i++) {
      const query = await fetch(`${ADMIN}/query`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', accept: 'application/json' },
        body: JSON.stringify({ query: delivered }),
      });
      const { rows } = (await query.json()) as { rows: { status: string }[] };
      if (rows[0]?.status === 'completed') return;
      if (i > 300) throw new Error(`delivery ${id} was not delivered: ${JSON.stringify(rows)}`);
      await sleep(50);
    }
  };
  const userTexts = async (customer: string) =>
    lines((await ingress.objectClient(agent('support'), customer).messages()).messages)
      .filter(([role]) => role === 'user')
      .map(([, text]) => text);

  // A webhook's delivery goes to the message being answered, as a steer
  // does (once, though Linear, say, sends it twice); with none answered, it
  // is a message of its own.
  const noras = await ingress
    .objectSendClient(agent('support'), 'nora')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-nora' }));
  await waits('support/nora');
  await hook('nora', 'She called again', 'd-1');
  await hook('nora', 'She called again', 'd-1');
  await ingress.objectClient(agent('support'), 'nora').signal({ name: 'approval', value: 'approved' });
  assert.equal(await ingress.result(noras), 'order 103: delivered / approved / refunded 103');
  const steeredHook = (await userTexts('nora'))[1];
  assert.match(steeredHook ?? '', /^<webhook name="hooks" delivery="d-1">\n[^]*"She called again"[^]*<\/webhook>$/);
  assert.equal((await userTexts('nora')).length, 2);
  await hook('nora', 'All good now', 'd-2');
  for (let i = 0; !(await userTexts('nora')).some((t) => t?.includes('All good now')); i++) {
    if (i > 300) throw new Error('the delivery to no turn was not answered');
    await sleep(50);
  }

  // A turn stopped before it takes a delivery answers it after, as a message
  // of its own (a steer it drops).
  const olive = ingress.objectClient(agent('support'), 'olive');
  const olives = await ingress
    .objectSendClient(agent('support'), 'olive')
    .chat('Refund order 103', clients.rpc.sendOpts({ idempotencyKey: 'msg-olive' }));
  await waits('support/olive');
  await hook('olive', 'Urgent', 'd-3');
  assert.deepEqual(await olive.stop(), { stopped: olives.invocationId });
  await assert.rejects(ingress.result(olives), /409/);
  for (let i = 0; !(await userTexts('olive')).some((t) => t?.includes('"Urgent"')); i++) {
    if (i > 300) throw new Error('the delivery to a stopped turn was not answered');
    await sleep(50);
  }

  // `follow` gives the conversation as it changes, without polling: the wait
  // for approval, then the answer, as the turn stores them.
  // A message comes once, and again once it changes: kept by id, as a UI keeps them.
  const stopFollowing = new AbortController();
  const seen: Followed[] = [];
  const kept = new Map<number, ChatMessage>();
  const last = () => [...kept.values()].toSorted((a, b) => a.id - b.id).at(-1);
  const following = (async () => {
    for await (const change of follow(ingress, agent('support'), 'mia', { signal: stopFollowing.signal })) {
      seen.push(change);
      for (const m of change.messages) kept.set(m.id, m);
      if (change.status.waiting.length > 0)
        await ingress.objectClient(agent('support'), 'mia').signal({ name: 'approval', value: 'approved' });
      if (change.status.answering === null && last()?.parts[0]?.type === 'text' && seen.length > 1) return;
    }
  })();
  await ingress.objectSendClient(agent('support'), 'mia').chat('Refund order 103');
  await following;
  stopFollowing.abort();
  assert.ok(
    seen.some((c) => c.status.answering !== null && c.status.waiting.length === 1),
    JSON.stringify(seen),
  );
  assert.deepEqual(last()?.parts[0], { type: 'text', text: 'order 103: delivered / approved / refunded 103' });
  assert.ok(seen.length < 20, `${seen.length} changes`);
  // A watch with nothing new answers at its timeout, with the same revision.
  const mia = ingress.objectClient(agent('support'), 'mia');
  const now = await mia.notifications();
  const started = Date.now();
  assert.deepEqual(await mia.watch({ revision: now.revision, timeoutSeconds: 1 }), now);
  assert.ok(Date.now() - started >= 900, `${Date.now() - started} ms`);

  // With no message answered, it is a message of its own.
  const own = await liam.steer('Thanks!');
  assert.equal(own.steered, false);
  assert.notEqual(own.invocationId, liams.invocationId);
  const owns = await fetch(`${INGRESS}/restate/invocation/${own.invocationId}/attach`);
  assert.ok(owns.ok, await owns.text());
  assert.equal(lines((await liam.messages()).messages).at(-2)?.[1], 'Thanks!');
  await post('support/ivy/signal', { name: 'approval', value: 'approved', keep: true });
  const keyed = await fetch(`${INGRESS}/support/ivy/chat/send`, {
    method: 'POST',
    headers: { 'content-type': 'text/plain', 'idempotency-key': 'msg-1' },
    body: 'Refund order 103',
  });
  assert.ok(keyed.ok, await keyed.text());
  const byKey = await fetch(`${INGRESS}/restate/invocation/support/ivy/chat/msg-1/attach`);
  assert.equal(await byKey.json(), 'order 103: delivered / approved / refunded 103');

  // A handler's terminal error is the tool's answer, for the model.
  assert.equal(await post('support/dave/chat', 'Refund order 999'), 'Error: no order 999');
  console.log('ok');
} finally {
  server.kill('SIGKILL');
  rmSync(dir, { recursive: true, force: true });
}
process.exit(0);

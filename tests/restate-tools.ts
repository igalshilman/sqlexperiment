// Restate's own tools (restate-agents' call, signal and workflow) against a
// real Restate server: the support agent of examples/support.ts on a scripted
// model, which looks the order up, waits for approval, then refunds.
//
// Checks: the agent waits for the signal (`pending` lists the wait, with
// what the model waits for; `status` and `messages` answer meanwhile) until
// it is resolved by its id, then answers
// with every tool's result; a signal no one waits for is dropped, unless it
// is kept; a cancelled wait leaves `pending`; a message may be plain text;
// and a handler's terminal error is the tool's answer.
//
//   RESTATE_SERVER=/path/to/restate-server node restate-tools.ts   (or make tools-test)
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as restate from '@restatedev/restate-sdk';
import { createModels } from '@earendil-works/pi-ai/models';
import { call, signal, signals, workflow, type Messages, type Status, type Waiting } from 'restate-agents';
import * as clients from '@restatedev/restate-sdk-clients';
import { agent } from 'restate-agents/client';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';
import { callTools, fakeProvider, say, textOf } from './fakes/pi.ts';

const SERVER = process.env.RESTATE_SERVER;
if (!SERVER) {
  console.error('RESTATE_SERVER is not set');
  process.exit(1);
}
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

// A workflow that calls the agent like any handler, as examples/triage.ts does.
const triage = restate.workflow({
  name: 'triage',
  handlers: {
    run: async (ctx: restate.WorkflowContext, ticket: string) => ctx.objectClient(support, ctx.key).chat(ticket),
  },
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
  await restate.serve({ services: [orders, refund, support, triage, signals], port: PORT });
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

  /** The waits of `agent`, once there is one. */
  const waits = async (agent: string) => {
    for (let i = 0; ; i++) {
      const pending = (await post(`${agent}/pending`)) as Waiting[];
      if (pending.length > 0) return pending;
      if (i > 300) throw new Error(`${agent} does not wait for the approval`);
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

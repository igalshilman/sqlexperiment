// Restate's own tools (restate-agents' call, signal and workflow) against a
// real Restate server: the support agent of examples/support.ts on a scripted
// model, which looks the order up, waits for approval, then refunds.
//
// Checks: the agent waits for the signal (`pending` lists it) until it is
// resolved, then answers with every tool's result; a signal resolved before
// the agent waits for it is kept for it; a message may be plain text; and a
// handler's terminal error is the tool's answer.
//
//   RESTATE_SERVER=/path/to/restate-server node restate-tools.ts   (or make tools-test)
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as restate from '@restatedev/restate-sdk';
import { createModels } from '@earendil-works/pi-ai/models';
import { call, signal, signals, workflow } from 'restate-agents';
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
    if (results.length === 1) return callTools([['wait_for_approval', {}]]);
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
    signal('approval', { description: 'Wait for a manager' }),
    workflow(refund, { description: 'Refund', input: z.object({ order: z.string() }) }),
  ],
});

const dir = mkdtempSync(join(tmpdir(), 'restate-tools-'));
const server = spawn(SERVER, ['--base-dir', dir], {
  env: { ...process.env, RESTATE_BIND_IP: '127.0.0.1', RESTATE_LISTEN_MODE: 'tcp' },
  stdio: 'ignore',
});
try {
  await restate.serve({ services: [orders, refund, support, signals], port: PORT });
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

  // The agent waits for the approval, and goes on once it comes.
  const answer = post('support/alice/chat', 'Refund order 103, please');
  for (let i = 0; ((await post('support/alice/pending')) as string[]).length === 0; i++) {
    if (i > 300) throw new Error('the agent does not wait for the approval');
    await sleep(100);
  }
  await post('support/alice/signal', { name: 'approval', value: 'approved by Kim' });
  assert.equal(await answer, 'order 103: delivered / approved by Kim / refunded 103');
  assert.deepEqual(await post('support/alice/pending'), []);
  // The conversation: what was said, without the tool calls.
  assert.deepEqual(await post('support/alice/messages'), [
    { role: 'user', text: 'Refund order 103, please' },
    { role: 'assistant', text: 'order 103: delivered / approved by Kim / refunded 103' },
  ]);

  // A signal resolved before the agent waits for it is kept for it.
  await post('support/bob/signal', { name: 'approval', value: 'approved early' });
  assert.equal(
    await post('support/bob/chat', 'Refund order 103'),
    'order 103: delivered / approved early / refunded 103',
  );

  // A message as plain text, as `curl -d` sends it.
  await post('support/carol/signal', { name: 'approval', value: 'approved' });
  const plain = await fetch(`${INGRESS}/support/carol/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: 'Refund order 103',
  });
  assert.equal(await plain.json(), 'order 103: delivered / approved / refunded 103');

  // A handler's terminal error is the tool's answer, for the model.
  assert.equal(await post('support/dave/chat', 'Refund order 999'), 'Error: no order 999');
  console.log('ok');
} finally {
  server.kill('SIGKILL');
  rmSync(dir, { recursive: true, force: true });
}
process.exit(0);

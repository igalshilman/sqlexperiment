// Restate's own tools (restate-agents' call, send, workflow and signal), run
// with a fake context that records the calls they make.
import * as restate from 'restate-objects';
import { call, object, send, signal, subagent, tool, workflow, type Tool } from 'restate-agents';
import { type AttemptContext, contextOf } from 'restate-objects/internal';
import { memoryDatabase } from 'restate-objects/test/support';
import { describe, expect, it } from 'vitest';
import { z } from 'zod';
import { settle } from '../src/calls.ts';
import { Transcript } from '../src/common.ts';

const orders = restate.service({ name: 'orders', handlers: { status: async (_ctx, _order: string) => 'shipped' } });
const carts = restate.object({
  name: 'carts',
  handlers: { add: async (_ctx: restate.SessionContext, _item: string) => 1 },
});
const refund = restate.workflow({
  name: 'refund',
  handlers: { run: async (_ctx: restate.WorkflowContext, _order: string) => 'refunded' },
});

/** A context, keyed `alice`, of invocation `inv1`, recording what is called and sent, with the object's database in `state`. */
function context(state = new Map<string, Uint8Array>()) {
  const made: Record<string, unknown>[] = [];
  // The SDK's context behind the handler's: what the agent calls and sends.
  const sdk = {
    key: 'alice',
    request: () => ({ id: 'inv1', target: { service: 'assistant', handler: 'chat', key: 'alice' } }),
    genericCall: async (c: Record<string, unknown>) => {
      made.push({ kind: 'call', ...c });
      return c.method === 'wait' ? 'approved' : `${String(c.method)} done`;
    },
    genericSend: (c: Record<string, unknown>) => void made.push({ kind: 'send', ...c }),
  };
  const ctx = contextOf(sdk as unknown as AttemptContext, memoryDatabase(state));
  return { made, ctx, sdk };
}

/** The tool `t` run as the model's tool call `call_1`, with `input` as the model gave it. */
function run(t: Tool, input: Record<string, unknown>) {
  const { made, ctx } = context();
  return t.run(input, { ctx, callId: 'call_1' }).then((result) => ({ result, made }));
}

const fields = (t: Tool) => Object.keys(z.toJSONSchema(t.input).properties ?? {});

describe('tool', () => {
  it('answers arguments that do not fit its input with what is wrong, and does not run', async () => {
    const t = subagent(object({ name: 'coder', handlers: { chat: async () => 'done' } }));
    const { result, made } = await run(t, { name: 'Fizz Buzz', task: 'Write it' });
    expect(result).toMatch(/^Error: the arguments do not fit the tool's input:/);
    expect(result).toMatch(/name/);
    expect(made).toEqual([]);
  });
});

describe('call', () => {
  it("calls a service's handler, once per tool call", async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    expect(t.name).toBe('orders_status');
    expect(fields(t)).toEqual(['order']);
    const { result, made } = await run(t, { order: '103' });
    expect(result).toBe('status done');
    expect(made).toMatchObject([
      { kind: 'call', service: 'orders', method: 'status', key: undefined, parameter: { order: '103' } },
    ]);
    expect(made[0]?.idempotencyKey).toBe('inv1:call_1');
  });

  it("asks the model for an object's key, unless it is given", async () => {
    const chosen = call(carts, 'add', { description: 'Add', input: z.object({ item: z.string() }) });
    expect(fields(chosen)).toEqual(['key', 'item']);
    expect((await run(chosen, { key: 'bob', item: 'pear' })).made[0]).toMatchObject({
      key: 'bob',
      parameter: { item: 'pear' },
    });

    const own = call(carts, 'add', {
      description: 'Add',
      input: z.object({ item: z.string() }),
      key: (ctx) => ctx.key,
    });
    expect(fields(own)).toEqual(['item']);
    expect((await run(own, { item: 'pear' })).made[0]).toMatchObject({ key: 'alice', parameter: { item: 'pear' } });
  });

  it('passes no input to a handler that takes none', async () => {
    const t = call(orders, 'status', { description: 'All' });
    expect(fields(t)).toEqual([]);
    expect((await run(t, {})).made[0]?.parameter).toBeUndefined();
  });
});

describe('send', () => {
  it('sends once per tool call, after a delay, and does not wait', async () => {
    const t = send(orders, 'status', {
      description: 'Later',
      input: z.object({ order: z.string() }),
      delay: { minutes: 5 },
    });
    const { result, made } = await run(t, { order: '103' });
    expect(result).toBe('sent to orders/status');
    expect(made).toMatchObject([
      { kind: 'send', service: 'orders', delay: { minutes: 5 }, idempotencyKey: 'inv1:call_1' },
    ]);
  });
});

describe('workflow', () => {
  it('runs a workflow of its own per tool call, to its result', async () => {
    const t = workflow(refund, { description: 'Refund', input: z.object({ order: z.string() }) });
    expect(t.name).toBe('refund');
    const { result, made } = await run(t, { order: '103' });
    expect(result).toBe('run done');
    expect(made).toMatchObject([
      { service: 'refund', method: 'run', key: 'alice-call_1', parameter: { order: '103' } },
    ]);
  });
});

describe('signal', () => {
  it("waits on the agent's signals object, once per tool call, for what the model says", async () => {
    const t = signal('approval', { description: 'Wait for a manager', input: z.object({ order: z.string() }) });
    expect(t.name).toBe('wait_for_approval');
    expect(fields(t)).toEqual(['order']);
    const { result, made } = await run(t, { order: '103' });
    expect(result).toBe('approved');
    expect(made).toMatchObject([
      {
        service: 'signals',
        key: 'assistant:alice',
        method: 'wait',
        parameter: { id: 'inv1:call_1', name: 'approval', input: { order: '103' } },
        idempotencyKey: 'inv1:call_1',
      },
    ]);
  });
});

describe('tool', () => {
  it('runs `run`, which may answer at once, with the tool call and its idempotency key', async () => {
    const t = tool({
      name: 'echo',
      description: 'Echo',
      input: z.object({ text: z.string() }),
      run: ({ text }, { callId, idempotencyKey }) => ({ text, callId, idempotencyKey }),
    });
    expect(JSON.parse((await run(t, { text: 'hi' })).result)).toEqual({
      text: 'hi',
      callId: 'call_1',
      idempotencyKey: 'inv1:call_1',
    });
  });
});

describe("Restate's tools", () => {
  it('make a call again with the call id of the attempt that first made it', async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    const state = new Map<string, Uint8Array>();
    // An attempt makes the call, and ends (a crash, a suspension) before it answers.
    const first = context(state);
    const made = Promise.withResolvers<void>();
    first.sdk.genericCall = (() => (made.resolve(), new Promise(() => {}))) as never;
    void t.run({ order: '103' }, { ctx: first.ctx, callId: 'call_1' });
    // Stored before it is made.
    await made.promise;
    // The next attempt's model makes it again, as a new tool call: it is the same call.
    const next = context(state);
    await t.run({ order: '103' }, { ctx: next.ctx, callId: 'call_2' });
    // With another input, it is another call.
    await t.run({ order: '104' }, { ctx: next.ctx, callId: 'call_3' });
    // Once answered, the same input is a call of its own.
    await t.run({ order: '103' }, { ctx: next.ctx, callId: 'call_4' });
    expect(next.made.map((m) => m.idempotencyKey)).toEqual(['inv1:call_1', 'inv1:call_3', 'inv1:call_4']);
  });

  it('make a call that answered again, until the framework stored its result', async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    const state = new Map<string, Uint8Array>();
    // An attempt makes the call, which answers; it crashes before the framework stores the result.
    const first = context(state);
    await t.run({ order: '103' }, { ctx: first.ctx, callId: 'call_1' });
    // The next attempt's model makes it again, as a new tool call: it is the same call.
    const next = context(state);
    await t.run({ order: '103' }, { ctx: next.ctx, callId: 'call_2' });
    // The framework stores its result (under the new id), which a publish settles.
    await settle(next.ctx, (id) => id === 'call_2');
    // A later attempt's model making it again makes a call of its own.
    const last = context(state);
    await t.run({ order: '103' }, { ctx: last.ctx, callId: 'call_3' });
    expect([...first.made, ...next.made, ...last.made].map((m) => m.idempotencyKey)).toEqual([
      'inv1:call_1',
      'inv1:call_1',
      'inv1:call_3',
    ]);
  });

  it('make a call that answered again, after the framework gave it an interruption error', async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    const state = new Map<string, Uint8Array>();
    const first = context(state);
    await t.run({ order: '103' }, { ctx: first.ctx, callId: 'call_1' });
    // Resumed, OpenCode gives the call an error, which is not its result: the model makes it again.
    const next = context(state);
    const transcript = new Transcript();
    transcript.call('call_1', 'status', { order: '103' });
    transcript.result('call_1', 'Tool execution was interrupted', true);
    await settle(next.ctx, (id) => transcript.answered(id));
    await t.run({ order: '103' }, { ctx: next.ctx, callId: 'call_2' });
    expect(next.made.map((m) => m.idempotencyKey)).toEqual(['inv1:call_1']);
  });

  it('make a call whose result is stored as a call of its own, in an attempt that made none yet', async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    const state = new Map<string, Uint8Array>();
    const first = context(state);
    await t.run({ order: '103' }, { ctx: first.ctx, callId: 'call_1' });
    // The framework stored its result, and the attempt ended before a publish settled it.
    const next = context(state);
    await settle(next.ctx, (id) => id === 'call_1');
    await t.run({ order: '103' }, { ctx: next.ctx, callId: 'call_2' });
    expect(next.made.map((m) => m.idempotencyKey)).toEqual(['inv1:call_2']);
  });

  it('make two calls of the same input in one attempt', async () => {
    const t = call(orders, 'status', { description: 'An order', input: z.object({ order: z.string() }) });
    const { ctx, made } = context();
    await Promise.all([
      t.run({ order: '103' }, { ctx, callId: 'call_1' }),
      t.run({ order: '103' }, { ctx, callId: 'call_2' }),
    ]);
    expect(made.map((m) => m.idempotencyKey)).toEqual(['inv1:call_1', 'inv1:call_2']);
  });
});

describe('subagent', () => {
  it('hands a task to the agent keyed after this one and the task, once per tool call', async () => {
    const t = subagent(carts, { description: 'a cart agent', handler: 'add' });
    expect(t.name).toBe('carts');
    expect(fields(t)).toEqual(['name', 'task']);
    const { result, made } = await run(t, { name: 'pears', task: 'add pears' });
    expect(result).toBe('add done');
    expect(made).toMatchObject([
      { service: 'carts', key: 'alice.pears', method: 'add', parameter: 'add pears', idempotencyKey: 'inv1:call_1' },
    ]);
  });
});

describe('tool', () => {
  it("answers a terminal error as the model's result, and lets any other fail the attempt", async () => {
    const lookup = tool({
      name: 'lookup',
      description: 'Look an order up',
      input: z.object({ order: z.string() }),
      run: async ({ order }) => {
        if (order === 'lost') throw new Error('connection reset');
        throw new restate.TerminalError(`no order ${order}`);
      },
    });
    expect((await run(lookup, { order: '7' })).result).toBe('Error: no order 7');
    await expect(run(lookup, { order: 'lost' })).rejects.toThrow('connection reset');
  });
});

/** An object with `handlers`, as a function that defines it. */
const named = (handlers: Record<string, () => Promise<void>>) => () => object({ name: 'assistant', handlers });

/** The names of the handlers of `o`, an object(). */
const handlersOf = (o: object) => Object.keys((o as { object: object }).object);

describe('object', () => {
  it("refuses handlers of its own named as restate-agents' own", () => {
    expect(named({ signal: async () => {} })).toThrow("`signal` is a handler of restate-agents' own");
    expect(named({ pending: async () => {} })).toThrow("`pending` is a handler of restate-agents' own");
    expect(named({ clear: async () => {} })).toThrow("`clear` is a handler of restate-agents' own");
    // @ts-expect-error: a handler every object has
    expect(() => object({ name: 'assistant', handlers: { signal: async () => {} } })).toThrow('`signal`');
  });

  it('has `clear` and `sync` only as an agent: one with a `messages` view', () => {
    const desk = object({ name: 'desk', handlers: { chat: async () => '' } });
    const agent = object({
      name: 'agent',
      handlers: { chat: async () => '' },
      views: { messages: async () => ({ messages: [], more: false, cursor: 0 }) },
    });
    expect(handlersOf(desk)).not.toContain('clear');
    expect(handlersOf(desk)).not.toContain('sync');
    expect(handlersOf(agent)).toEqual(expect.arrayContaining(['clear', 'sync', 'erase']));
  });
});

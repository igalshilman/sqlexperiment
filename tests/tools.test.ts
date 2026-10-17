// Restate's own tools (restate-agents' call, send, workflow and signal), run
// with a fake context that records the calls they make.
import * as restate from '@restatedev/restate-sdk';
import { call, object, send, signal, subagent, tool, workflow, type Tool } from 'restate-agents';
import { describe, expect, it } from 'vitest';
import { z } from 'zod';

const orders = restate.service({ name: 'orders', handlers: { status: async (_ctx, _order: string) => 'shipped' } });
const carts = restate.object({
  name: 'carts',
  handlers: { add: async (_ctx: restate.ObjectContext, _item: string) => 1 },
});
const refund = restate.workflow({
  name: 'refund',
  handlers: { run: async (_ctx: restate.WorkflowContext, _order: string) => 'refunded' },
});

/** A context, keyed `alice`, of invocation `inv1`, recording what is called and sent. */
function context() {
  const made: Record<string, unknown>[] = [];
  const ctx = {
    key: 'alice',
    request: () => ({ id: 'inv1', target: { service: 'assistant', handler: 'chat', key: 'alice' } }),
    genericCall: async (c: Record<string, unknown>) => (made.push({ kind: 'call', ...c }), `${String(c.method)} done`),
    genericSend: (c: Record<string, unknown>) => void made.push({ kind: 'send', ...c }),
    objectClient: (definition: { name: string }, key: string) => ({
      wait: async (name: string, opts: restate.Opts<string, unknown>) => {
        made.push({ kind: 'call', service: definition.name, key, method: 'wait', parameter: name, ...opts.getOpts() });
        return 'approved';
      },
    }),
  };
  return { made, ctx: ctx as unknown as restate.SqlObjectContext };
}

/** The tool `t` run as the model's tool call `call_1`, with `input`. */
function run(t: Tool, input: Record<string, unknown>) {
  const { made, ctx } = context();
  return t.run(t.input.parse(input), { ctx, callId: 'call_1' }).then((result) => ({ result, made }));
}

const fields = (t: Tool) => Object.keys(z.toJSONSchema(t.input).properties ?? {});

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
  it("waits on the agent's signals object for the signal, once per tool call", async () => {
    const t = signal('approval', { description: 'Wait for a manager' });
    expect(t.name).toBe('wait_for_approval');
    const { result, made } = await run(t, {});
    expect(result).toBe('approved');
    expect(made).toMatchObject([
      {
        service: 'signals',
        key: 'assistant:alice',
        method: 'wait',
        parameter: 'approval',
        idempotencyKey: 'inv1:call_1',
      },
    ]);
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

describe('object', () => {
  it('refuses handlers of its own named as the ones every object has', () => {
    expect(named({ signal: async () => {} })).toThrow('every object has a `signal` handler already');
    expect(named({ pending: async () => {} })).toThrow('every object has a `pending` handler already');
    // @ts-expect-error: a handler every object has
    expect(() => object({ name: 'assistant', handlers: { signal: async () => {} } })).toThrow('`signal`');
  });
});

// The agents' companion (`signals`): the turn it keeps from `start` to
// `close`, which `status`, `stop`, `steer` and `deliver` read. Its handlers
// run on a fake context, which keeps the state and records what they send.
import * as restate from '@restatedev/restate-sdk';
import { signals } from 'restate-agents';
import { describe, expect, it } from 'vitest';

const agent = { service: 'assistant', key: 'alice' };
const A = 'inv_1a';
const B = 'inv_1b';

/** A companion's state and what its handlers sent (texts sent as messages) and cancelled. */
function companion() {
  const state = new Map<string, unknown>();
  const sent: string[] = [];
  const cancelled: string[] = [];
  const ctx = {
    key: 'assistant:alice',
    get: async (name: string) => structuredClone(state.get(name)),
    set: (name: string, value: unknown) => void state.set(name, structuredClone(value)),
    clear: (name: string) => void state.delete(name),
    genericSend: ({ parameter }: { parameter: string }) => {
      sent.push(parameter);
      return { invocationId: Promise.resolve(`inv_sent${sent.length}`) };
    },
    resolveAwakeable: () => {},
    invocation: (id: { toString(): string }) => ({ cancel: () => void cancelled.push(String(id)) }),
  };
  const handlers = (signals as unknown as { object: Record<string, (ctx: unknown, input?: unknown) => unknown> })
    .object;
  const run = <T>(name: string, input?: unknown) => handlers[name]!(ctx, input) as Promise<T>;
  return { run, sent, cancelled };
}

const turn = (invocationId: string) => ({ invocationId, since: '2026-10-31T00:00:00.000Z' });
const close = (id: string, cancelled = false, taken: string[] = []) => ({ agent, turn: id, taken, cancelled });
const answering = async (c: ReturnType<typeof companion>) =>
  (await c.run<{ answering: { invocationId: string } | null }>('status')).answering?.invocationId ?? null;

describe("an agent's companion", () => {
  it('knows the turn from its start to its close; a retried start keeps it', async () => {
    const c = companion();
    expect(await answering(c)).toBeNull();
    await c.run('start', { agent, turn: turn(A) });
    await c.run('start', { agent, turn: { ...turn(A), since: 'later' } });
    expect((await c.run<{ answering: { since: string } }>('status')).answering.since).toBe(turn(A).since);
    await c.run('close', close(A));
    expect(await answering(c)).toBeNull();
  });

  it('steers a message to the turn, and sends one when no turn is answered', async () => {
    const c = companion();
    expect(await c.run('steer', { agent, text: 'idle' })).toMatchObject({ steered: false });
    await c.run('start', { agent, turn: turn(A) });
    expect(await c.run('steer', { agent, text: 'more' })).toEqual({ invocationId: A, steered: true });
    expect(await c.run('steering', A)).toEqual([{ id: `${A}:steer:1`, text: 'more' }]);
    // Not taken: a message of its own once the turn closes.
    await c.run('close', close(A));
    expect(c.sent).toEqual(['idle', 'more']);
  });

  it('drops what a cancelled turn did not take, but for a delivery', async () => {
    const c = companion();
    await c.run('start', { agent, turn: turn(A) });
    await c.run('steer', { agent, text: 'steered' });
    await c.run('deliver', { agent, text: 'delivered' });
    await c.run('close', close(A, true));
    expect(c.sent).toEqual(['delivered']);
  });

  it('closes a turn that never closed (a killed invocation) as the next starts: its deliveries are answered', async () => {
    const c = companion();
    await c.run('start', { agent, turn: turn(A) });
    await c.run('steer', { agent, text: 'steered' });
    await c.run('deliver', { agent, text: 'delivered' });
    await c.run('start', { agent, turn: turn(B) });
    expect(c.sent).toEqual(['delivered']);
    expect(await answering(c)).toBe(B);
    // Steered to the new turn, not queued on the dead one.
    expect(await c.run('steer', { agent, text: 'next' })).toEqual({ invocationId: B, steered: true });
    // The dead turn's close, if it ever came, changes nothing.
    await c.run('close', close(A));
    expect(c.sent).toEqual(['delivered']);
    expect(await answering(c)).toBe(B);
  });

  it('stops the turn it answers, and nothing when none is', async () => {
    const c = companion();
    expect(await c.run('stop')).toEqual({ stopped: null });
    const id = restate.InvocationIdParser.fromString('inv_1aGuEfH1fC0F1tDaHYyy2FfUD7DhkVvjSl').toString();
    await c.run('start', { agent, turn: turn(id) });
    expect(await c.run('stop')).toEqual({ stopped: id });
    expect(c.cancelled).toEqual([id]);
  });
});

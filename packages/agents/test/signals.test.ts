// The agents' companion (`signals`): the turn it keeps from `start` to
// `close`, which `status` reads (and the `stop` view, through `status`). Its
// handlers run on a fake context, which keeps the state.
import { signals } from 'restate-agents';
import { describe, expect, it } from 'vitest';

const A = 'inv_1a';
const B = 'inv_1b';

/** A companion's state, and a run of its handlers. */
function companion() {
  const state = new Map<string, unknown>();
  const ctx = {
    key: 'assistant:alice',
    get: async (name: string) => structuredClone(state.get(name)),
    set: (name: string, value: unknown) => void state.set(name, structuredClone(value)),
    clear: (name: string) => void state.delete(name),
    resolveAwakeable: () => {},
  };
  const handlers = (signals as unknown as { object: Record<string, (ctx: unknown, input?: unknown) => unknown> })
    .object;
  const run = <T>(name: string, input?: unknown) => handlers[name]!(ctx, input) as Promise<T>;
  return { run };
}

const turn = (invocationId: string) => ({ invocationId, since: '2026-10-31T00:00:00.000Z' });
const answering = async (c: ReturnType<typeof companion>) =>
  (await c.run<{ answering: { invocationId: string } | null }>('status')).answering?.invocationId ?? null;

describe("an agent's companion", () => {
  it('knows the turn from its start to its close; a retried start keeps it', async () => {
    const c = companion();
    expect(await answering(c)).toBeNull();
    await c.run('start', turn(A));
    await c.run('start', { ...turn(A), since: 'later' });
    expect((await c.run<{ answering: { since: string } }>('status')).answering.since).toBe(turn(A).since);
    await c.run('close', A);
    expect(await answering(c)).toBeNull();
  });

  it('takes the next turn after one that never closed (a killed invocation), whose close changes nothing', async () => {
    const c = companion();
    await c.run('start', turn(A));
    await c.run('start', turn(B));
    expect(await answering(c)).toBe(B);
    await c.run('close', A);
    expect(await answering(c)).toBe(B);
  });
});

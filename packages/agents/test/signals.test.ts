// The agents' companion (`signals`): the turn it keeps from `start` to
// `close`, which `status` reads (and the `stop` view, through `status`), and
// watches (`awaitTurn`). Its handlers run on a fake context, which keeps the
// state and records what they send to the companion itself.
import * as restate from '@restatedev/restate-sdk';
import { signals } from 'restate-agents';
import { describe, expect, it, vi } from 'vitest';

// The fake context's promises are plain ones: race them as such.
vi.spyOn(restate.RestatePromise, 'race').mockImplementation(
  (promises) => Promise.race(promises) as unknown as restate.RestatePromise<never>,
);

const A = 'inv_1a';
const B = 'inv_1b';

/**
 * A promise as the SDK gives one, as far as the companion uses it: `map`,
 * which gets its value or its failure; but a cancellation (of the invocation
 * that awaits it) rejects it, as the SDK's does, before `map`.
 */
const restatePromise = <T>(
  promise: Promise<T>,
): Promise<T> & { map: (f: (value?: T, failure?: Error) => unknown) => unknown } =>
  Object.assign(promise, {
    map: <U>(f: (value?: T, failure?: Error) => U) =>
      restatePromise(
        promise.then(
          (value) => f(value),
          (failure: Error) => {
            if (failure instanceof restate.CancelledError) throw failure;
            return f(undefined, failure);
          },
        ),
      ),
  });

const never = () => new Promise<never>(() => {});

/**
 * A companion's state, a run of its handlers, what they sent to the
 * companion itself, and the awakeables they rejected. `ends` says how each
 * turn ends, for `awaitTurn`: by default, it never does; with `watched`, its
 * watch is over at once.
 */
function companion(ends: Record<string, Promise<unknown>> = {}, { watched = false } = {}) {
  const state = new Map<string, unknown>();
  const own: { method: string; input: unknown }[] = [];
  const rejected: string[] = [];
  const ctx = {
    key: 'assistant:alice',
    get: async (name: string) => structuredClone(state.get(name)),
    set: (name: string, value: unknown) => void state.set(name, structuredClone(value)),
    clear: (name: string) => void state.delete(name),
    resolveAwakeable: () => {},
    rejectAwakeable: (awakeable: string) => void rejected.push(awakeable),
    objectSendClient: () =>
      new Proxy({}, { get: (_, method: string) => (input: unknown) => void own.push({ method, input }) }),
    invocation: (id: string) => ({ attach: () => restatePromise(ends[id] ?? never()) }),
    sleep: () => restatePromise(watched ? Promise.resolve() : never()),
  };
  const handlers = (signals as unknown as { object: Record<string, (ctx: unknown, input?: unknown) => unknown> })
    .object;
  const run = <T>(name: string, input?: unknown) => handlers[name]!(ctx, input) as Promise<T>;
  return { run, own, rejected };
}

const wait = (id: string, awakeable: string) => ({ id, name: 'approval', since: '', awakeable });
const revision = async (c: ReturnType<typeof companion>) =>
  (await c.run<{ revision: number }>('notifications')).revision;

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
  it('watches each turn it starts, once: a retried start watches nothing more', async () => {
    const c = companion();
    await c.run('start', turn(A));
    await c.run('start', turn(A));
    expect(c.own).toEqual([{ method: 'awaitTurn', input: A }]);
  });

  it('closes a turn that fails (a killed one), and nothing for one that ends as it should', async () => {
    const killed = Promise.reject(new restate.TerminalError('killed', { errorCode: 409 }));
    killed.catch(() => {});
    const c = companion({ [A]: killed, [B]: Promise.resolve('answered') });
    await c.run('awaitTurn', A);
    await c.run('awaitTurn', B);
    expect(c.own).toEqual([{ method: 'close', input: A }]);
  });

  it('hands a turn still answered after its watch to a new watch, and stops watching a closed one', async () => {
    const c = companion({}, { watched: true });
    await c.run('start', turn(A));
    c.own.length = 0;
    await c.run('awaitTurn', A);
    expect(c.own).toEqual([{ method: 'awaitTurn', input: A }]);
    await c.run('close', A);
    await c.run('awaitTurn', A);
    expect(c.own).toHaveLength(1);
  });

  it('closes nothing when it is cancelled itself: the turn may still run', async () => {
    const cancelled = Promise.reject(new restate.CancelledError());
    cancelled.catch(() => {});
    const c = companion({ [A]: cancelled });
    await expect(c.run('awaitTurn', A)).rejects.toBeInstanceOf(restate.CancelledError);
    expect(c.own).toEqual([]);
  });

  it('publishes a close that changes the status, once', async () => {
    const c = companion();
    await c.run('start', turn(A));
    const before = await revision(c);
    await c.run('close', A);
    await c.run('close', A);
    expect(await revision(c)).toBe(before + 1);
    expect(await answering(c)).toBeNull();
  });

  it("forgets a closed turn's waits (a killed turn leaves them), and ends them; not another's", async () => {
    const c = companion();
    await c.run('start', turn(A));
    await c.run('listen', wait(`${A}:call_1`, 'awk_a'));
    await c.run('listen', wait(`${B}:call_1`, 'awk_b'));
    await c.run('close', A);
    expect((await c.run<{ id: string }[]>('pending')).map((w) => w.id)).toEqual([`${B}:call_1`]);
    expect(c.rejected).toEqual(['awk_a']);
  });

  it('forgets the waits of a turn that never closed as the next starts', async () => {
    const c = companion();
    await c.run('start', turn(A));
    await c.run('listen', wait(`${A}:call_1`, 'awk_a'));
    await c.run('start', turn(B));
    expect(await c.run('pending')).toEqual([]);
    expect(c.rejected).toEqual(['awk_a']);
  });
});

// `ctx.step`, without a Restate server: each attempt has the SDK's `ctx.db`
// and `ctx.step` over the object's state in memory. Attempts with the same
// request id are one invocation's; a crash is an attempt abandoned, whose
// commits stay.
import * as restate from 'restate-fns';
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from './support.ts';

const store = restate.object({
  name: 'Store',
  handlers: { reserve: async (_ctx: restate.ObjectContext, _item: string) => {} },
});

/** An object's state, and attempts of its invocations over it. */
function object() {
  const map = new Map<string, Uint8Array>();
  return {
    /** An attempt of invocation `id`, recording its sends. */
    attempt(id = 'inv') {
      const sent: restate.GenericSend<Uint8Array>[] = [];
      return { ...sqlAttempt({ map, id, send: (s) => sent.push(s) }), sent };
    },
    /** Whether the table of steps exists. */
    async steps() {
      const a = this.attempt('look');
      const rows = await a.db.query("SELECT name FROM sqlite_master WHERE name = 'restate_steps'");
      a.abort();
      return rows.length > 0;
    },
  };
}

/** A step's body that fails the test if it runs. */
const never = async () => {
  throw new Error('ran again');
};

/** A request to a model, counted. */
function model() {
  let calls = 0;
  return {
    get calls() {
      return calls;
    },
    ask: async (prompt: string) => `answer ${++calls} to ${prompt}`,
  };
}

describe('ctx.step', () => {
  it('runs once per invocation: a retried attempt gets the stored result', async () => {
    const o = object();
    const m = model();
    const crashed = o.attempt();
    expect(await crashed.step('ask', () => m.ask('hi'))).toBe('answer 1 to hi');
    crashed.abort(); // the attempt crashes after the step committed
    const retried = o.attempt();
    expect(await retried.end(() => retried.step('ask', () => m.ask('hi')))).toBe('answer 1 to hi');
    expect(m.calls).toBe(1);
  });

  it('commits its SQL and sends with its result, and none of them if it fails', async () => {
    const o = object();
    const failed = o.attempt();
    const step = (a: ReturnType<typeof o.attempt>, fail: boolean) =>
      a.step('order', async ({ db, tx }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS orders (item TEXT)');
        await db.execute("INSERT INTO orders VALUES ('book')");
        tx.objectSendClient(store, 'a').reserve('book');
        if (fail) throw new Error('no');
        return 7;
      });
    await expect(step(failed, true)).rejects.toThrow('no');
    // Not committed: the name is free, and nothing was stored or sent.
    await expect(step(failed, false)).resolves.toBe(7);
    failed.abort();
    expect(failed.sent).toHaveLength(1);
    const retried = o.attempt();
    await retried.end(async () => {
      expect(await step(retried, false)).toBe(7);
      expect(await retried.db.query('SELECT item FROM orders')).toEqual([['book']]);
    });
    // The send of the commit, made again with its key from the outbox: Restate drops it.
    expect(retried.sent.map((s) => s.idempotencyKey)).toEqual(failed.sent.map((s) => s.idempotencyKey));
  });

  it('keeps steps until the invocation ends, then drops their table', async () => {
    const o = object();
    const a = o.attempt();
    // A retryable error: Restate retries the invocation, which keeps its steps.
    await expect(
      a.end(() => a.step('ask', async () => 1).then(() => Promise.reject(new Error('later')))),
    ).rejects.toThrow('later');
    expect(await o.steps()).toBe(true);
    const b = o.attempt();
    await expect(
      b.end(async () => {
        expect(await b.step('ask', async () => 2)).toBe(1);
        throw new restate.TerminalError('done');
      }),
    ).rejects.toThrow('done');
    expect(await o.steps()).toBe(false);
  });

  it("is an invocation's own: the next one runs it again, and drops what a killed one left", async () => {
    const o = object();
    const killed = o.attempt('first');
    await killed.step('ask', async () => 'first');
    killed.abort(); // and never retried
    const next = o.attempt('second');
    expect(await next.end(() => next.step('ask', async () => 'second'))).toBe('second');
    expect(await o.steps()).toBe(false);
  });

  it('runs steps without SQL concurrently', async () => {
    const o = object();
    const a = o.attempt();
    const both = Promise.withResolvers<void>();
    let started = 0;
    const step = (name: string) =>
      a.step(name, async () => {
        if (++started === 2) both.resolve();
        await both.promise; // each waits for the other to start
        return name;
      });
    expect(await a.end(() => Promise.all([step('a'), step('b')]))).toEqual(['a', 'b']);
  });

  it('keeps undefined, null and objects', async () => {
    const o = object();
    const a = o.attempt();
    await a.step('nothing', async () => undefined);
    await a.step('null', async () => null);
    await a.step('object', async () => ({ text: 'hi', n: [1, 2] }));
    a.abort();
    const b = o.attempt();
    await b.end(async () => {
      expect(await b.step('nothing', never)).toBeUndefined();
      expect(await b.step('null', never)).toBeNull();
      expect(await b.step('object', never)).toEqual({ text: 'hi', n: [1, 2] });
    });
  });

  it('refuses a name the invocation used, the session inside a step, and a shared handler', async () => {
    const o = object();
    const a = o.attempt();
    await a.end(async () => {
      await a.step('ask', async () => 1);
      await expect(a.step('ask', async () => 2)).rejects.toBeInstanceOf(restate.TerminalError);
    });
    // Inside a step's body, its own db: the session would wait for the step.
    const b = o.attempt('other');
    await b.end(async () => {
      const nested = await b.step('outer', async ({ db }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS t (v INTEGER)');
        return Promise.all([
          b.db.query('SELECT 1').catch((e: unknown) => (e as Error).message),
          b.step('inner', async () => 1).catch((e: unknown) => (e as Error).message),
        ]);
      });
      expect(nested).toEqual([
        'inside a transaction, use the transaction handle its body gets',
        'inside a transaction, use the transaction handle its body gets',
      ]);
    });
    const reader = sqlAttempt({ map: new Map(), shared: true });
    await expect(reader.step('ask', async () => 1)).rejects.toThrow('ctx.step is for exclusive handlers');
  });
});

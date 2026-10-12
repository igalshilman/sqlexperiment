// The agent's handler without a Restate server: a fake `ctx` with a database
// in memory and a ledger that records its calls, through crashes of the
// handler. A crash is an attempt that stops in a tool call and is abandoned:
// what it committed stays, as with a crashed process, and the runner gets no
// chance to save anything on the way out. `npm run test:claude` (or make
// test-claude).
import { open } from 'sqlite-kv';
import { describe, expect, it } from 'vitest';
import { agent } from './agent.mjs';
import { teller } from './teller.mjs';

const prompt = agent.object.prompt;

/** A conversation's object: its database lives across attempts, like the object's state. */
async function conversation(key) {
  const map = new Map();
  const store = {
    get: async (k) => map.get(k),
    set: (k, v) => map.set(k, Uint8Array.from(v)),
    clear: (k) => map.delete(k),
    sync: async () => {},
  };
  // `crashNext`: the next transfer is recorded, and its attempt stops there.
  // `failNext`: the next transfer is recorded, and then throws.
  const ledger = { transfers: new Map(), calls: [], crashNext: 0, failNext: 0, crashed: null };
  let n = 0;
  return {
    ledger,
    /** One attempt of the handler, as Restate makes it, with request id `requestId`. */
    async attempt(requestId, input) {
      const db = await open(`claude-agent-${process.pid}-${key}-${n++}`, store);
      const ctx = {
        key,
        request: () => ({ id: requestId }),
        objectClient: () => ({
          async transfer(args, opts) {
            ledger.calls.push({ name: 'transfer', id: args.id, key: opts.getOpts().idempotencyKey });
            if (!ledger.transfers.has(args.id)) ledger.transfers.set(args.id, args);
            if (ledger.crashNext > 0) {
              ledger.crashNext--;
              ledger.crashed();
              return new Promise(() => {});
            }
            if (ledger.failNext > 0) {
              ledger.failNext--;
              throw new Error('crash after the transfer');
            }
          },
          async audit() {
            return { accounts: 8, total: 8000, transfers: ledger.transfers.size, inconsistent: 0 };
          },
        }),
        sqlSession: async (body) => {
          try {
            return await body(db);
          } finally {
            await db.close();
          }
        },
      };
      return prompt(ctx, input);
    },
    /** An attempt that crashes in its next transfer. */
    async crash(requestId, input) {
      ledger.crashNext = 1;
      const crashed = new Promise((resolve) => (ledger.crashed = resolve));
      const attempt = this.attempt(requestId, input);
      await Promise.race([crashed, attempt.then(() => Promise.reject(new Error('did not crash')))]);
    },
  };
}

describe('the Claude agent', () => {
  it('answers a prompt once, by its id', async () => {
    const c = await conversation('a');
    const first = await c.attempt('inv1', { id: 'p1', text: 'Please move 5 from 1 to 2' });
    expect(first).toEqual({ answer: 'Done: moved 5 from account 1 to account 2' });
    // Asked again, in a new invocation: the same answer, no new transfer.
    expect(await c.attempt('inv2', { id: 'p1', text: 'Please move 5 from 1 to 2' })).toEqual(first);
    expect(c.ledger.calls).toHaveLength(1);
    expect([...c.ledger.transfers.keys()]).toEqual(['ClaudeAgent/a/toolu_1']);
    // The next prompt goes on in the same thread.
    const audit = await c.attempt('inv3', { id: 'p2', text: 'audit the books' });
    expect(audit.answer).toMatch(/^Done: .*"transfers":1/);
  });

  it('resumes a crashed attempt from its last step, with the same tool call', async () => {
    const c = await conversation('b');
    const calls = { model: 0 };
    const reply = teller.reply;
    teller.reply = (messages) => {
      calls.model++;
      return reply(messages);
    };
    try {
      const input = { id: 'p1', text: 'Please move 7 from 3 to 4' };
      await c.crash('inv1', input);
      expect(calls.model).toBe(1);
      // The retried attempt runs the tool again, not the model.
      expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 7 from account 3 to account 4' });
      expect(calls.model).toBe(2); // the answer after the tool
    } finally {
      teller.reply = reply;
    }
    // The same transfer id and idempotency key both times: the money moved once.
    expect(c.ledger.calls).toEqual([
      { name: 'transfer', id: 'ClaudeAgent/b/toolu_1', key: 'inv1:toolu_1' },
      { name: 'transfer', id: 'ClaudeAgent/b/toolu_1', key: 'inv1:toolu_1' },
    ]);
    expect(c.ledger.transfers.size).toBe(1);
  });

  it("fails the attempt on a tool's error, rather than hand it to the model", async () => {
    const c = await conversation('d');
    c.ledger.failNext = 1;
    const input = { id: 'p1', text: 'Please move 3 from 5 to 6' };
    await expect(c.attempt('inv1', input)).rejects.toThrow('crash after the transfer');
    expect(await c.attempt('inv1', input)).toEqual({ answer: 'Done: moved 3 from account 5 to account 6' });
    expect(c.ledger.transfers.size).toBe(1);
  });

  it('finishes an interrupted prompt before starting the next one', async () => {
    const c = await conversation('c');
    await c.crash('inv1', { id: 'p1', text: 'Please move 1 from 0 to 1' });
    // Its caller gave up; the next prompt comes.
    const second = await c.attempt('inv2', { id: 'p2', text: 'Please move 2 from 0 to 1' });
    expect(second).toEqual({ answer: 'Done: moved 2 from account 0 to account 1' });
    expect(await c.attempt('inv3', { id: 'p1', text: 'Please move 1 from 0 to 1' })).toEqual({
      answer: 'Done: moved 1 from account 0 to account 1',
    });
    expect(c.ledger.transfers.size).toBe(2);
  });
});

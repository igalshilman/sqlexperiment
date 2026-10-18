// Test support, for the examples' tests without a Restate server: an object's
// state in memory, and a fake object for an agent's handler, with a ledger
// that records its calls, through crashes of the handler.
import { expect } from 'vitest';
import { randomUUID } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import { type Steer, tool, type Tool } from 'restate-agents';
import type { Store } from 'sqlite-kv';
import { z } from 'zod';
import type { Audit, Transfer } from './sql/ledger.ts';

/**
 * A sqlite-kv store over `map`. Give the same map to several databases for an
 * object's state that lives across attempts. Writes through a closed `fence`
 * throw, as Restate fences an attempt that a newer one replaced.
 */
export function memoryStore(map = new Map<string, Uint8Array>(), fence = { closed: false }): Store {
  const check = () => {
    if (fence.closed) throw new Error('fenced');
  };
  return {
    get: async (key) => map.get(key),
    set: (key, value) => {
      check();
      map.set(key, Uint8Array.from(value));
    },
    clear: (key) => {
      check();
      map.delete(key);
    },
    sync: async () => {},
  };
}

/**
 * The `signals` object of an agent's tests, as far as steering and following
 * go: the messages steered to each turn, the turns closed, and how often the
 * conversation was published.
 */
export class Steering {
  /** The conversation's revision: how often it was published. */
  revision = 0;
  /** The messages steered to each turn, by its invocation id. */
  readonly turns = new Map<string, Steer[]>();
  /** Each turn closed, with the ids of the messages it took. */
  readonly closed: { turn: string; taken: string[]; cancelled: boolean }[] = [];

  /** Steer turn `turn` (an invocation id) with `text`, as the object's `steer` does. */
  steer(turn: string, text: string) {
    const messages = this.turns.get(turn) ?? [];
    this.turns.set(turn, [...messages, { id: `${turn}:steer:${messages.length + 1}`, text }]);
  }

  /** The messages of turn `turn` that it did not take: they are answered as messages of their own. */
  left(turn: string) {
    const close = this.closed.findLast((c) => c.turn === turn);
    return (this.turns.get(turn) ?? []).filter((m) => !close?.taken.includes(m.id)).map((m) => m.text);
  }
}

/** The steering of each object's state. */
const steerings = new WeakMap<Map<string, Uint8Array>, Steering>();

/** The steering of the object of state `map`. */
export const steeringOf = (map: ReadonlyMap<string, Uint8Array>) => {
  const object = map as Map<string, Uint8Array>;
  if (!steerings.has(object)) steerings.set(object, new Steering());
  return steerings.get(object)!;
};

/** An attempt of a handler, for {@link sqlAttempt}. */
export interface AttemptOptions {
  /** The object's state, which lives across attempts. */
  map: Map<string, Uint8Array>;
  /** Closed, the attempt's writes and sends throw, as a fenced attempt's. */
  fence?: { closed: boolean };
  /** The object's key. */
  key?: string;
  /** The request id. */
  id?: string;
  /** Each send the attempt makes. */
  send?: (send: restate.GenericSend<Uint8Array>) => void;
  /** Before each of the attempt's state requests. */
  onRequest?: () => void;
  /** A shared handler's attempt: its `ctx.db` reads beside the owner's, and writes nothing. */
  shared?: boolean;
}

/**
 * `ctx.db` and `ctx.transaction` of an attempt, the SDK's own, over `map`, and
 * `end`, which runs the handler and ends the attempt's session as the SDK
 * does. Each attempt is a database of its own in this process (a namespace
 * of its own), so that one a crash abandoned does not share the page cache.
 */
export function sqlAttempt({
  map,
  fence = { closed: false },
  key = 'k',
  id = 'inv',
  send = () => {},
  onRequest = () => {},
  shared = false,
}: AttemptOptions) {
  const check = () => {
    onRequest();
    if (fence.closed) throw new Error('fenced');
  };
  const end = Promise.withResolvers<void>();
  end.promise.catch(() => {});
  const target = { service: `test-${randomUUID()}` };
  const ctx = {
    handlerKind: shared ? 2 : 1, // shared, or exclusive
    journalReplay: false,
    key,
    request: () => ({ id, target }),
    invocationEndPromise: end,
    defaultSerde: restate.serde.json,
    get: async (k: string) => {
      onRequest();
      return map.get(k) ?? null;
    },
    // The store's reads, left out of the journal.
    ephemeralStateGet: async (k: string) => {
      onRequest();
      return map.get(k) ?? null;
    },
    set: (k: string, value: Uint8Array) => {
      check();
      map.set(k, Uint8Array.from(value));
    },
    clear: (k: string) => {
      check();
      map.delete(k);
    },
    run: async () => {},
    genericSend: (s: restate.GenericSend<Uint8Array>) => {
      check();
      send(s);
    },
  };
  const attempt = new restate.internal.SqlAttempt(ctx);
  const cancelled = Promise.withResolvers<void>();
  const steering = steeringOf(map);
  return {
    db: attempt.db,
    request: ctx.request,
    /** The `signals` object, as steering calls it. */
    objectClient: (definition: { name: string }) => {
      if (definition.name !== 'signals') throw new Error(`no ${definition.name} in this test`);
      return {
        steering: async (turn: string) => steering.turns.get(turn) ?? [],
        publish: async () => ({ revision: ++steering.revision }),
        progress: async (turn: string) => {
          steering.revision++;
          return steering.turns.get(turn) ?? [];
        },
      };
    },
    objectSendClient: (definition: { name: string }) => {
      if (definition.name !== 'signals') throw new Error(`no ${definition.name} in this test`);
      return { close: (close: Steering['closed'][number]) => void steering.closed.push(close) };
    },
    /** Restate's explicit cancellation (`restate.internal.asInternal(ctx).cancellation()`), which `cancel` signals. */
    cancellation: () => cancelled.promise,
    cancelPreviousCalls: async () => [],
    /** Cancel the invocation, as Restate does. */
    cancel: () => cancelled.resolve(),
    /** `ctx.get` and `ctx.set` of the object's state, as JSON. */
    get: async <T>(name: string): Promise<T | null> => {
      const value = await ctx.get(name);
      return value === null ? null : (JSON.parse(new TextDecoder().decode(value)) as T);
    },
    set: (name: string, value: unknown) => ctx.set(name, new TextEncoder().encode(JSON.stringify(value))),
    clear: (name: string) => ctx.clear(name),
    transaction: <T>(body: (t: restate.SqlTransaction) => Promise<T>) => attempt.transaction(body),
    /** `handler`'s result, once its session ended. */
    end: <T>(handler: () => Promise<T>) => restate.internal.withSql(handler, () => attempt),
    /** The attempt ends, as when Restate fences or suspends it. */
    abort: () => end.reject(new Error('the attempt ended')),
  };
}

/** A prompt to an agent, and its answer. */
/** A chat handler, such as an integration's `chat()`: a message in, the answer out. */
export type PromptHandler = (ctx: restate.SqlObjectContext, message: string) => Promise<string>;

/** The fake ledger of a conversation. */
export interface FakeLedger {
  /** Each transfer, once, by id. */
  transfers: Map<string, Transfer>;
  /** Each call to `transfer`, with its idempotency key. */
  calls: { name: 'transfer'; id: string; key: string | undefined }[];
  /** The next transfers are recorded, and their attempts stop there. */
  crashNext: number;
  /** The next transfers are recorded, and then throw. */
  failNext: number;
  /** Called when an attempt stops in a transfer. */
  crashed: () => void;
}

/**
 * An agent's conversation, the object with key `key` whose handler is
 * `handler`: its database lives across attempts, like the object's state.
 *
 * A crash is an attempt that stops in a transfer and is abandoned: what it
 * stored stays, as with a crashed process, and the handler gets no chance to
 * store anything on the way out. Like Restate, the next attempt fences it, so
 * it stores nothing afterwards.
 */
export function conversation(
  handler: PromptHandler,
  key: string,
  from?: { state: ReadonlyMap<string, Uint8Array>; ledger: FakeLedger },
) {
  // From another conversation: a copy of its state.
  const map = new Map<string, Uint8Array>(from?.state);
  let fence: { closed: boolean } = { closed: false };
  // Ends the attempt before: Restate fences it once the next one starts.
  let abort: (() => void) | undefined;
  const ledger: FakeLedger = { transfers: new Map(), calls: [], crashNext: 0, failNext: 0, crashed: () => {} };
  // The books as an attempt calls them: a fenced (crashed) attempt's calls
  // never reach them, as a dead process's do not.
  const booksOf = (attempt: { closed: boolean }) => ({
    async transfer(transfer: Transfer, opts: restate.Opts<Transfer, Transfer>) {
      if (attempt.closed) return new Promise<Transfer>(() => {});
      ledger.calls.push({ name: 'transfer', id: transfer.id, key: opts.getOpts().idempotencyKey });
      if (!ledger.transfers.has(transfer.id)) ledger.transfers.set(transfer.id, transfer);
      const applied = ledger.transfers.get(transfer.id)!;
      if (ledger.crashNext > 0) {
        ledger.crashNext--;
        ledger.crashed();
        return new Promise<Transfer>(() => {});
      }
      if (ledger.failNext > 0) {
        ledger.failNext--;
        throw new Error('crash after the transfer');
      }
      return applied;
    },
    async audit(): Promise<Audit> {
      return { accounts: 8, total: 8000, transfers: ledger.transfers.size, inconsistent: 0 };
    },
  });
  if (from) for (const [id, transfer] of from.ledger.transfers) ledger.transfers.set(id, transfer);
  return {
    ledger,
    /** The object's state. */
    state: map as ReadonlyMap<string, Uint8Array>,
    /**
     * One attempt of the handler, as Restate makes it, with request id
     * `requestId`; with `crashAt`, it crashes at that state request (from 1),
     * and is abandoned there.
     */
    async attempt(requestId: string, prompt: string, crashAt?: number) {
      fence.closed = true;
      fence = { closed: false };
      const current = fence;
      abort?.();
      let requests = 0;
      const crashed = Promise.withResolvers<'crashed'>();
      const onRequest = () => {
        if (++requests !== crashAt) return;
        current.closed = true;
        crashed.resolve('crashed');
        throw new Error('crash at a state request');
      };
      const sql = sqlAttempt({ map, fence, key, id: requestId, onRequest });
      abort = sql.abort;
      const ctx = {
        key,
        ...sql,
        objectClient: (definition: { name: string }) =>
          definition.name === 'signals' ? sql.objectClient(definition) : booksOf(current),
      };
      const answer = sql.end(() => handler(ctx as unknown as restate.SqlObjectContext, prompt));
      if (crashAt === undefined) return answer;
      answer.catch(() => {});
      return Promise.race([crashed.promise, answer]);
    },
    /**
     * A shared handler of the object (such as `messages`), with `input`, in an
     * attempt of its own: it reads beside an attempt that runs, if any.
     */
    async read<T, I>(other: (ctx: restate.SqlObjectContext, input: I) => Promise<T>, input?: I, requestId = 'read') {
      const sql = sqlAttempt({ map, key, id: requestId, shared: true });
      const ctx = { key, ...sql };
      return sql.end(() => other(ctx as unknown as restate.SqlObjectContext, input as I));
    },
    /** An attempt that crashes in its next transfer. */
    async crash(requestId: string, prompt: string) {
      ledger.crashNext = 1;
      const crashed = new Promise<void>((resolve) => (ledger.crashed = resolve));
      const attempt = this.attempt(requestId, prompt);
      await Promise.race([crashed, attempt.then(() => Promise.reject(new Error('did not crash')))]);
    },
  };
}

/** A tool call of `messages`, by `name`, with any id. */
const toolCall = (name: string, input: unknown, output: string) => ({
  type: 'tool',
  id: expect.any(String),
  name,
  input,
  output,
});

/** The messages of the bank conversation of the agents' tests: a transfer, then an audit. */
export const SAID = [
  { role: 'user', parts: [{ type: 'text', text: 'Please move 5 from 1 to 2' }] },
  {
    role: 'assistant',
    parts: [
      { type: 'text', text: 'On it.' },
      toolCall('transfer', { from: 1, to: 2, amount: 5 }, 'moved 5 from account 1 to account 2'),
    ],
  },
  { role: 'assistant', parts: [{ type: 'text', text: 'Done: moved 5 from account 1 to account 2' }] },
  { role: 'user', parts: [{ type: 'text', text: 'audit the books' }] },
  {
    role: 'assistant',
    parts: [toolCall('audit', {}, '{"accounts":8,"total":8000,"transfers":1,"inconsistent":0}')],
  },
  {
    role: 'assistant',
    parts: [{ type: 'text', text: 'Done: {"accounts":8,"total":8000,"transfers":1,"inconsistent":0}' }],
  },
].map((m, id) => ({ id, role: m.role, parts: m.parts }));

/**
 * What goes wrong when `prompt` crashes at a state request of a message and
 * is retried, at every `stride`th request (CRASH_STRIDE, default 3), some at
 * once, until it answers without crashing: each should answer it, with one transfer, and one
 * stored prompt in `messages`. From a conversation with a message before.
 *
 * `steered`: the message is steered ({@link STEERED}) before it starts, and
 * the steering is taken (in `messages`) or left (answered as a message of
 * its own), exactly once, whatever the answer.
 */
export async function crashedAnywhere(
  prompt: PromptHandler,
  messages: (
    ctx: restate.SqlObjectContext,
    request: object,
  ) => Promise<{ messages: { role: string; parts: { type: string; text?: string }[] }[] }>,
  {
    stride = Number(process.env.CRASH_STRIDE ?? 3),
    concurrency = 8,
    steered = false,
  }: {
    /** Crash at every `stride`th state request. */
    stride?: number;
    /** How many crash points at once; 1 for a model fake that is not safe to share. */
    concurrency?: number;
    /** Steer the message before it starts. */
    steered?: boolean;
  } = {},
) {
  const before = conversation(prompt, 'anywhere');
  await before.attempt('inv0', 'Please move 1 from 3 to 4');
  const crashAt = async (n: number) => {
    const c = conversation(prompt, 'anywhere', before);
    if (steered) steeringOf(c.state).steer('inv1', STEERED);
    if ((await c.attempt('inv1', 'Please move 5 from 1 to 2', n)) !== 'crashed') return 'finished';
    const answer = await Promise.race([
      c.attempt('inv1', 'Please move 5 from 1 to 2'),
      new Promise((resolve) => setTimeout(() => resolve('no answer'), 10_000)),
    ]).catch((error: unknown) => String(error));
    const users = (await c.read(messages, {})).messages.filter((m) => m.role === 'user');
    if (steered) {
      const taken = users.filter((m) => m.parts.some((p) => p.text === STEERED)).length;
      const left = steeringOf(c.state).left('inv1').length;
      const ok = typeof answer === 'string' && c.ledger.transfers.size === 2 && taken + left === 1;
      return ok ? 'ok' : `${n}: ${String(answer)}, ${c.ledger.transfers.size} transfers, ${taken} taken, ${left} left`;
    }
    const ok =
      answer === 'Done: moved 5 from account 1 to account 2' && c.ledger.transfers.size === 2 && users.length === 2;
    return ok ? 'ok' : `${n}: ${String(answer)}, ${c.ledger.transfers.size} transfers, ${users.length} prompts`;
  };
  // A batch at a time, until one answers without crashing.
  const wrong: string[] = [];
  for (let from = 1, done = false; !done; from += concurrency * stride) {
    const batch = await Promise.all(Array.from({ length: concurrency }, (_, i) => crashAt(from + i * stride)));
    wrong.push(...batch.filter((r) => r !== 'ok' && r !== 'finished'));
    done = batch.includes('finished');
  }
  return wrong;
}

/** A transfer whose invocation is cancelled while it runs (while it waits for an approval, say). */
export const cancelledTransfer = tool({
  name: 'transfer',
  description: 'Move money.',
  input: z.object({ from: z.number(), to: z.number(), amount: z.number() }),
  run: () => {
    throw new restate.CancelledError();
  },
});

/**
 * `chat` (with {@link cancelledTransfer}) on an object of its own: its first
 * message (a transfer) cancelled, then a second one, `Hello`. What the first
 * failed with, the second's answer, and the conversation then: each
 * message's parts.
 */
export async function afterCancel(
  chat: PromptHandler,
  messages: (ctx: restate.SqlObjectContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
) {
  const map = new Map<string, Uint8Array>();
  const invoke = <I, O>(
    handler: (ctx: restate.SqlObjectContext, input: I) => Promise<O>,
    id: string,
    input: I,
    shared = false,
  ) => {
    const sql = sqlAttempt({ map, key: 'cancel', id, shared });
    const ctx = { key: 'cancel', ...sql };
    return sql.end(() => handler(ctx as unknown as restate.SqlObjectContext, input));
  };
  const failed = await invoke(chat, 'inv1', 'Please move 5 from 1 to 2').then(
    () => undefined,
    (error: unknown) => error,
  );
  const answer = await invoke(chat, 'inv2', 'Hello');
  const page = await invoke(messages, 'read', {}, true);
  return { failed, answer, parts: page.messages.map((m) => m.parts) };
}

/**
 * `chat` (with `tools`) on an object of its own, its first message (a
 * transfer) cancelled while `transfer` runs (when it calls `started`), not
 * while a Restate call is pending: as during a model request.
 */
export async function cancelledWhileWorking(
  chat: (tools: Tool[]) => PromptHandler,
  messages: (ctx: restate.SqlObjectContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
) {
  const map = new Map<string, Uint8Array>();
  const started = Promise.withResolvers<void>();
  const transfer = tool({
    name: 'transfer',
    description: 'Move money.',
    input: z.object({ from: z.number(), to: z.number(), amount: z.number() }),
    run: async () => {
      started.resolve();
      return new Promise<never>(() => {}); // as long as it takes
    },
  });
  const handler = chat([transfer]);
  const attempt = (id: string, shared = false) => {
    const sql = sqlAttempt({ map, key: 'cancel', id, shared });
    return { sql, ctx: { key: 'cancel', ...sql } as unknown as restate.SqlObjectContext };
  };
  const first = attempt('inv1');
  const answering = first.sql.end(() => handler(first.ctx, 'Please move 5 from 1 to 2'));
  await started.promise;
  first.sql.cancel();
  const failed = await answering.then(
    () => undefined,
    (error: unknown) => error,
  );
  const second = attempt('inv2');
  const answer = await second.sql.end(() => handler(second.ctx, 'Hello'));
  const read = attempt('read', true);
  const page = await read.sql.end(() => messages(read.ctx, {}));
  return { failed, answer, parts: page.messages.map((m) => m.parts) };
}

/** What {@link steeredWhileWorking} steers a turn with. */
export const STEERED = 'Only from account 1, please';

/**
 * `chat` (with `tools`) on an object of its own, its message (a transfer)
 * steered ({@link STEERED}) while its tool `transfer` runs: its answer, the
 * conversation then (each message's parts), the steering that the turn did
 * not take, which is answered as messages of their own, and how often the
 * turn published its conversation.
 */
export async function steeredWhileWorking(
  chat: (tools: Tool[]) => PromptHandler,
  messages: (ctx: restate.SqlObjectContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
) {
  const map = new Map<string, Uint8Array>();
  const steering = steeringOf(map);
  const transfer = tool({
    name: 'transfer',
    description: 'Move money.',
    input: z.object({ from: z.number(), to: z.number(), amount: z.number() }),
    run: (_transfer, { ctx }) => {
      steering.steer(ctx.request().id, STEERED);
      return 'moved 5 from account 1 to account 2';
    },
  });
  const attempt = (id: string, shared = false) => {
    const sql = sqlAttempt({ map, key: 'steer', id, shared });
    return { sql, ctx: { key: 'steer', ...sql } as unknown as restate.SqlObjectContext };
  };
  const turn = attempt('inv1');
  const answer = await turn.sql.end(() => chat([transfer])(turn.ctx, 'Please move 5 from 1 to 2'));
  const read = attempt('read', true);
  const page = await read.sql.end(() => messages(read.ctx, {}));
  return {
    answer,
    parts: withoutIds(page.messages.map((m) => m.parts)),
    left: steering.left('inv1'),
    published: steering.revision,
  };
}

/** `parts` without their tool calls' ids, which differ from run to run. */
export const withoutIds = (parts: unknown[][]) =>
  parts.map((message) =>
    message.map((part) => {
      if (typeof part !== 'object' || part === null) return part;
      return Object.fromEntries(Object.entries(part).filter(([key]) => key !== 'id'));
    }),
  );

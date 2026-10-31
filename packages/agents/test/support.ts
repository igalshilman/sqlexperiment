// Test support for the agents, without a Restate server: an attempt of an
// agent's handler with the `signals` object, and a conversation with a ledger
// that records its calls, through crashes of the handler.
import { expect } from 'vitest';
import * as restate from 'restate-session-objects';
import { type Steer, tool, type Tool } from 'restate-agents';
import { type AttemptContext, contextOf } from 'restate-session-objects/internal';
import { type AttemptOptions, sqlAttempt as baseAttempt } from 'restate-session-objects/test/support';
import { z } from 'zod';
import type { Audit, Transfer } from 'restate-session-objects/test/ledger';

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
  /** The message being answered, from its turn's start to its close. */
  answering: { invocationId: string; since: string } | null = null;

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

/** A call or send an attempt makes, as the SDK gets it (`ctx.rpc`, `ctx.send`). */
export interface Made {
  service: string;
  method: string;
  key?: string;
  parameter: unknown;
  idempotencyKey?: string;
}

/** A call as the SDK gets it, but its serdes. */
const made = (c: Made): Made => ({
  service: c.service,
  method: c.method,
  key: c.key,
  parameter: c.parameter,
  idempotencyKey: c.idempotencyKey,
});

/**
 * An attempt of a handler, restate-session-objects's own, with the `signals` object
 * of steering over `map`: `ctx`, a handler's context, and `view`, a view's.
 * `route` answers the calls to anything but `signals`.
 */
export function sqlAttempt(options: AttemptOptions & { route?: (call: Made) => Promise<unknown> }) {
  const steering = steeringOf(options.map);
  const base = baseAttempt(options);
  // The `signals` object, as the agents call it.
  const signals: Record<string, (input: never) => unknown> = {
    steering: (turn: string) => steering.turns.get(turn) ?? [],
    publish: () => ({ revision: ++steering.revision }),
    progress: (turn: string) => {
      steering.revision++;
      return steering.turns.get(turn) ?? [];
    },
    start: (turn: { invocationId: string; since: string }) => {
      if (steering.answering?.invocationId !== turn.invocationId) steering.answering = turn;
    },
    close: (close: Steering['closed'][number]) => {
      if (steering.answering?.invocationId === close.turn) steering.answering = null;
      steering.closed.push(close);
    },
  };
  const route = (c: Made) => {
    if (c.service !== 'signals') {
      if (options.route) return options.route(made(c));
      throw new Error(`no ${c.service} in this test`);
    }
    const method = signals[c.method];
    if (!method) throw new Error(`no signals.${c.method} in this test`);
    return method(c.parameter as never);
  };
  const sdk = {
    key: options.key ?? 'k',
    request: () => ({
      ...base.request(),
      target: { service: options.service ?? 'agent', key: options.key ?? 'k', handler: 'chat' },
    }),
    genericCall: async (c: Made) => route(c),
    genericSend: (c: Made) => void route(c),
    cancellation: base.cancellation,
    cancelPreviousCalls: base.cancelPreviousCalls,
  } as unknown as AttemptContext;
  return { ...base, ctx: contextOf(sdk, base.db), view: contextOf(sdk, base.db, true) };
}

/** A handler: a function, or one with options (as `chat()` gives). */
export type Entry<F> = F | { handler: F };

/** A chat handler, or a handler with options (`chat()`'s): its function. */
export const handlerOf = <F>(entry: F | { handler: F }): F =>
  typeof entry === 'function' ? entry : (entry as { handler: F }).handler;

export type PromptHandler =
  | ((ctx: restate.SessionContext, message: string) => Promise<string>)
  | { handler: (ctx: restate.SessionContext, message: string) => Promise<string> };

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
    async transfer(transfer: Transfer, idempotencyKey: string | undefined) {
      if (attempt.closed) return new Promise<Transfer>(() => {});
      ledger.calls.push({ name: 'transfer', id: transfer.id, key: idempotencyKey });
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
      const books = booksOf(current);
      const route = async ({ method, parameter, idempotencyKey }: Made) =>
        method === 'transfer' ? books.transfer(parameter as Transfer, idempotencyKey) : books.audit();
      const sql = sqlAttempt({ map, fence, key, id: requestId, onRequest, route });
      abort = sql.abort;
      const answer = sql.end(() => handlerOf(handler)(sql.ctx, prompt));
      if (crashAt === undefined) return answer;
      answer.catch(() => {});
      return Promise.race([crashed.promise, answer]);
    },
    /**
     * A view of the object (such as `messages`), with `input`, in an
     * attempt of its own: it reads beside an attempt that runs, if any.
     */
    async read<T, I>(other: (ctx: restate.ViewContext, input: I) => Promise<T>, input?: I, requestId = 'read') {
      const sql = sqlAttempt({ map, key, id: requestId, shared: true });
      return sql.end(() => other(sql.view, input as I));
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
    ctx: restate.ViewContext,
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
  messages: (ctx: restate.ViewContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
) {
  const map = new Map<string, Uint8Array>();
  const invoke = (id: string, input: string) => {
    const sql = sqlAttempt({ map, key: 'cancel', id });
    return sql.end(() => handlerOf(chat)(sql.ctx, input));
  };
  const failed = await invoke('inv1', 'Please move 5 from 1 to 2').then(
    () => undefined,
    (error: unknown) => error,
  );
  const answer = await invoke('inv2', 'Hello');
  const read = sqlAttempt({ map, key: 'cancel', id: 'read', shared: true });
  const page = await read.end(() => messages(read.view, {}));
  return { failed, answer, parts: page.messages.map((m) => m.parts) };
}

/**
 * `chat` (with `tools`) on an object of its own, its first message (a
 * transfer) cancelled while `transfer` runs (when it calls `started`), not
 * while a Restate call is pending: as during a model request.
 */
export async function cancelledWhileWorking(
  chat: (tools: Tool[]) => PromptHandler,
  messages: (ctx: restate.ViewContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
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
  const handler = handlerOf(chat([transfer]));
  const attempt = (id: string, shared = false) => {
    const sql = sqlAttempt({ map, key: 'cancel', id, shared });
    return { sql, ctx: sql.ctx, view: sql.view };
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
  const page = await read.sql.end(() => messages(read.view, {}));
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
  messages: (ctx: restate.ViewContext, request: object) => Promise<{ messages: { parts: unknown[] }[] }>,
  { service }: { service?: string } = {},
) {
  const map = new Map<string, Uint8Array>();
  const steering = steeringOf(map);
  const transfer = tool({
    name: 'transfer',
    description: 'Move money.',
    input: z.object({ from: z.number(), to: z.number(), amount: z.number() }),
    run: (_transfer, { ctx }) => {
      steering.steer(ctx.request.id, STEERED);
      return 'moved 5 from account 1 to account 2';
    },
  });
  const attempt = (id: string, shared = false) => {
    const sql = sqlAttempt({ map, key: 'steer', id, shared, service });
    return { sql, ctx: sql.ctx, view: sql.view };
  };
  const turn = attempt('inv1');
  const answer = await turn.sql.end(() => handlerOf(chat([transfer]))(turn.ctx, 'Please move 5 from 1 to 2'));
  const read = attempt('read', true);
  const page = await read.sql.end(() => messages(read.view, {}));
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

/**
 * `chat` (with `tools`) on an object of its own, asked to transfer 5 with a
 * `transfer` that takes at most 3: the model's arguments do not fit, which
 * is the model's to fix, not a failure of the attempt. Its answer (after the
 * tool's error), and whether `transfer` ran.
 */
export async function invalidArguments(chat: (tools: Tool[]) => PromptHandler) {
  let ran = false;
  const small = tool({
    name: 'transfer',
    description: 'Move money: at most 3.',
    input: z.object({ from: z.number(), to: z.number(), amount: z.number().max(3) }),
    run: () => {
      ran = true;
      return 'moved';
    },
  });
  const map = new Map<string, Uint8Array>();
  const sql = sqlAttempt({ map, key: 'invalid', id: 'inv1' });
  const answer = await sql.end(() => handlerOf(chat([small]))(sql.ctx, 'Please move 5 from 1 to 2'));
  return { answer, ran };
}

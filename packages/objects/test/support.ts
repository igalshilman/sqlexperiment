// Test support: an object's state in memory, and an attempt of a handler over
// it, with restate-session-objects's own `ctx.db` and `ctx.transaction`, through crashes
// and fencing, without a Restate server.
import { randomUUID } from 'node:crypto';
import * as restate from 'restate-session-objects';
import {
  asDatabase,
  contextOf,
  lazyDatabase,
  open,
  Attempt,
  type SqlHost,
  withAttempt,
} from 'restate-session-objects/internal';
import type { AttemptContext, Store } from 'restate-session-objects/internal';

/**
 * A handler's `ctx.db` over `map` without the SDK, opened on its first call:
 * an attempt's, which sends nothing. Give the same map to the next attempt's.
 */
export function memoryDatabase(map = new Map<string, Uint8Array>()): restate.Database {
  return lazyDatabase(async () => asDatabase(await open(`memory-${randomUUID()}`, memoryStore(map))));
}

/**
 * An SQL store over `map`. Give the same map to several databases for an
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
    keys: async () => [...map.keys()],
  };
}

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
  /** The object's name (by default, one of its own), such as one with a schema. */
  service?: string;
}

/**
 * `ctx.db` and `ctx.transaction` of an attempt, restate-session-objects's own, over
 * `map`, and `end`, which runs the handler and ends the attempt's session
 * as an object's handler does. Each attempt is a database of its own in this
 * process (a namespace of its own), so that one a crash abandoned does not
 * share the page cache.
 */
export function sqlAttempt({
  map,
  fence = { closed: false },
  key = 'k',
  id = 'inv',
  send = () => {},
  onRequest = () => {},
  shared = false,
  service = `test-${randomUUID()}`,
}: AttemptOptions) {
  const check = () => {
    onRequest();
    if (fence.closed) throw new Error('fenced');
  };
  const end = Promise.withResolvers<void>();
  end.promise.catch(() => {});
  const target = { service };
  const ctx = {
    shared,
    key,
    request: () => ({ id, target }),
    invocationEndPromise: end,
    defaultSerde: restate.serde.json,
    get: async (k: string) => {
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
    stateKeys: async () => {
      onRequest();
      return [...map.keys()];
    },
    run: async () => {},
    genericSend: (s: restate.GenericSend<Uint8Array>) => {
      check();
      send(s);
    },
  } satisfies SqlHost;
  const attempt = new Attempt(ctx);
  const cancelled = Promise.withResolvers<void>();
  return {
    db: attempt.db,
    request: ctx.request,
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
    /** The handler's context, over this attempt's `ctx.db`. */
    ctx: contextOf(sdkOf(ctx), attempt.db),
    /** A view's context, over this attempt's `ctx.db`. */
    view: contextOf(sdkOf(ctx), attempt.db, true),
    /** `handler`'s result, once its session ended. */
    end: <T>(handler: () => Promise<T>) => withAttempt(handler, attempt),
    /** The attempt ends, as when Restate fences or suspends it. */
    abort: () => end.reject(new Error('the attempt ended')),
  };
}

/** A call or send a {@link fakeContext} was asked for. */
export interface Made {
  service: string;
  method: string;
  key?: string;
  parameter: unknown;
  idempotencyKey?: string;
  delay?: number;
}

/** A parameter as JSON: a send made again from the outbox has it as bytes. */
const decoded = (parameter: unknown, inputSerde?: unknown) =>
  parameter instanceof Uint8Array && inputSerde === restate.serde.binary
    ? parameter.length === 0
      ? undefined
      : (JSON.parse(new TextDecoder().decode(parameter)) as unknown)
    : parameter;

/** The SDK's context behind a test's attempt `ctx`: its key, request and sends. */
const sdkOf = (ctx: SqlHost & { key: string }): AttemptContext =>
  ({
    key: ctx.key,
    request: () => ctx.request(),
    genericSend: (send: restate.GenericSend<Uint8Array>) => ctx.genericSend(send),
    genericCall: () => {
      throw new Error('this attempt calls nothing');
    },
  }) as unknown as AttemptContext;

/** A call or send as a test sees it. */
const made = (c: restate.GenericCall<unknown, unknown> | restate.GenericSend<unknown>): Made => ({
  service: c.service,
  method: c.method,
  ...(c.key !== undefined ? { key: c.key } : {}),
  parameter: decoded(c.parameter, c.inputSerde),
  ...(c.idempotencyKey !== undefined ? { idempotencyKey: c.idempotencyKey } : {}),
  ...('delay' in c && c.delay !== undefined ? { delay: Number(c.delay) } : {}),
});

/** The context of one attempt of a session object's handler, for `invoke` of restate-session-objects/internal. */
export function fakeContext({
  map,
  key = 'k',
  id = 'inv',
  service,
  calls = [],
  sends = [],
  answer = () => null,
}: {
  map: Map<string, Uint8Array>;
  key?: string;
  id?: string;
  /** The object's name: its definition's. */
  service: string;
  calls?: Made[];
  sends?: Made[];
  /** The answer to each call. */
  answer?: (call: Made) => unknown;
}): AttemptContext {
  const end = Promise.withResolvers<void>();
  end.promise.catch(() => {});
  const ctx = {
    key,
    request: () => ({ id, target: { service, key, handler: 'h' } }),
    invocationEndPromise: end,
    defaultSerde: restate.serde.json,
    get: async (k: string) => map.get(k) ?? null,
    set: (k: string, value: Uint8Array) => void map.set(k, Uint8Array.from(value)),
    clear: (k: string) => void map.delete(k),
    stateKeys: async () => [...map.keys()],
    run: async () => {},
    genericSend: (s: restate.GenericSend<unknown>) => {
      sends.push(made(s));
      return {} as restate.InvocationHandle;
    },
    genericCall: (c: restate.GenericCall<unknown, unknown>) => {
      const call = made(c);
      calls.push(call);
      return Promise.resolve(answer(call));
    },
  };
  return ctx as unknown as AttemptContext;
}

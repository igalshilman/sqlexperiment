// Test support: an object's state in memory, and an attempt of a handler over
// it, with restate-objects's own `ctx.db` and `ctx.transaction`, through crashes
// and fencing, without a Restate server.
import { randomUUID } from 'node:crypto';
import * as restate from 'restate-objects';
import { SqlAttempt, type SqlHost, withSql } from 'restate-objects/internal';
import type { Store } from 'restate-objects/internal';

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
 * `ctx.db` and `ctx.transaction` of an attempt, restate-objects's own, over
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
  const attempt = new SqlAttempt(ctx);
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
    transaction: <T>(body: (t: restate.SqlTransaction) => Promise<T>) => attempt.transaction(body),
    /** `handler`'s result, once its session ended. */
    end: <T>(handler: () => Promise<T>) => withSql(handler, attempt),
    /** The attempt ends, as when Restate fences or suspends it. */
    abort: () => end.reject(new Error('the attempt ended')),
  };
}

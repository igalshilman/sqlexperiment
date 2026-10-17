// Test support, for the examples' tests without a Restate server: an object's
// state in memory, and a fake object for an agent's handler, with a ledger
// that records its calls, through crashes of the handler.
import { randomUUID } from 'node:crypto';
import * as restate from '@restatedev/restate-sdk';
import type { Store } from 'sqlite-kv';
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
}

/**
 * `ctx.db` and `ctx.transaction` of an attempt, the SDK's own, over `map`, and
 * `end`, which runs the handler and ends the attempt's session as the SDK
 * does. Each attempt is a database of its own in this process (a namespace
 * of its own), so that one a crash abandoned does not share the page cache.
 */
export function sqlAttempt({ map, fence = { closed: false }, key = 'k', id = 'inv', send = () => {} }: AttemptOptions) {
  const check = () => {
    if (fence.closed) throw new Error('fenced');
  };
  const end = Promise.withResolvers<void>();
  end.promise.catch(() => {});
  const target = { service: `test-${randomUUID()}` };
  const ctx = {
    handlerKind: 1, // exclusive
    journalReplay: false,
    key,
    request: () => ({ id, target }),
    invocationEndPromise: end,
    defaultSerde: restate.serde.json,
    get: async (k: string) => map.get(k) ?? null,
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
  return {
    db: attempt.db,
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
export function conversation(handler: PromptHandler, key: string) {
  const map = new Map<string, Uint8Array>();
  let fence = { closed: false };
  // Ends the attempt before: Restate fences it once the next one starts.
  let abort: (() => void) | undefined;
  const ledger: FakeLedger = { transfers: new Map(), calls: [], crashNext: 0, failNext: 0, crashed: () => {} };
  const books = {
    async transfer(transfer: Transfer, opts: restate.Opts<Transfer, Transfer>) {
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
  };
  return {
    ledger,
    /** One attempt of the handler, as Restate makes it, with request id `requestId`. */
    async attempt(requestId: string, prompt: string) {
      fence.closed = true;
      fence = { closed: false };
      abort?.();
      const sql = sqlAttempt({ map, fence, key, id: requestId });
      abort = sql.abort;
      const ctx = {
        key,
        request: () => ({ id: requestId }),
        objectClient: () => books,
        db: sql.db,
        transaction: sql.transaction,
      };
      return sql.end(() => handler(ctx as unknown as restate.SqlObjectContext, prompt));
    },
    /** Another handler of the object, without input (such as `messages`), in an attempt of its own. */
    async read<T>(other: (ctx: restate.SqlObjectContext) => Promise<T>, requestId = 'read') {
      fence.closed = true;
      fence = { closed: false };
      abort?.();
      const sql = sqlAttempt({ map, fence, key, id: requestId });
      abort = sql.abort;
      const ctx = { key, request: () => ({ id: requestId }), db: sql.db, transaction: sql.transaction };
      return sql.end(() => other(ctx as unknown as restate.SqlObjectContext));
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

/** What `messages` gives after the bank conversation of the agents' tests: a transfer, then an audit. */
export const SAID = [
  { role: 'user', text: 'Please move 5 from 1 to 2' },
  { role: 'assistant', text: 'On it.' },
  { role: 'assistant', text: 'Done: moved 5 from account 1 to account 2' },
  { role: 'user', text: 'audit the books' },
  { role: 'assistant', text: 'Done: {"accounts":8,"total":8000,"transfers":1,"inconsistent":0}' },
];

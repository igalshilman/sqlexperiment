// Test support, for the examples' tests without a Restate server: an object's
// state in memory, and a fake object for an agent's handler, with a ledger
// that records its calls, through crashes of the handler.
import { randomUUID } from 'node:crypto';
import type * as restate from '@restatedev/restate-sdk';
import { open, type Store } from 'sqlite-kv';
import type { Audit, Transfer } from './node/ledger.ts';

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

/** A prompt to an agent, and its answer. */
export interface Prompt {
  id: string;
  text: string;
}
export type PromptHandler = (ctx: restate.UnjournaledObjectContext, prompt: Prompt) => Promise<{ answer: string }>;

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
  const name = randomUUID();
  let fence = { closed: false };
  let n = 0;
  const ledger: FakeLedger = { transfers: new Map(), calls: [], crashNext: 0, failNext: 0, crashed: () => {} };
  const books = {
    async transfer(transfer: Transfer, opts: restate.Opts<Transfer, boolean>) {
      ledger.calls.push({ name: 'transfer', id: transfer.id, key: opts.getOpts().idempotencyKey });
      if (!ledger.transfers.has(transfer.id)) ledger.transfers.set(transfer.id, transfer);
      if (ledger.crashNext > 0) {
        ledger.crashNext--;
        ledger.crashed();
        return new Promise<boolean>(() => {});
      }
      if (ledger.failNext > 0) {
        ledger.failNext--;
        throw new Error('crash after the transfer');
      }
      return true;
    },
    async audit(): Promise<Audit> {
      return { accounts: 8, total: 8000, transfers: ledger.transfers.size, inconsistent: 0 };
    },
  };
  return {
    ledger,
    /** One attempt of the handler, as Restate makes it, with request id `requestId`. */
    async attempt(requestId: string, prompt: Prompt) {
      fence.closed = true;
      fence = { closed: false };
      const db = await open(`${name}-${n++}`, memoryStore(map, fence));
      const ctx = {
        key,
        request: () => ({ id: requestId }),
        objectClient: () => books,
        sqlSession: async <T>(body: (db: restate.SqlSession) => Promise<T>) => {
          try {
            return await body(db);
          } finally {
            await db.close();
          }
        },
      };
      return handler(ctx as unknown as restate.UnjournaledObjectContext, prompt);
    },
    /** An attempt that crashes in its next transfer. */
    async crash(requestId: string, prompt: Prompt) {
      ledger.crashNext = 1;
      const crashed = new Promise<void>((resolve) => (ledger.crashed = resolve));
      const attempt = this.attempt(requestId, prompt);
      await Promise.race([crashed, attempt.then(() => Promise.reject(new Error('did not crash')))]);
    },
  };
}

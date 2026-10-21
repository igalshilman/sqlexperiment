// An attempt's SQL session: the object's database, kept in its state through
// the sqlite-kv addon, opened on the handler's first use of `ctx.db` or
// `ctx.transaction` and saved when the handler ends; and `ctx.transaction(body)`,
// one transaction of it with the sends it commits with.
import { Buffer } from 'node:buffer';
import { randomUUID } from 'node:crypto';
import type * as restate from '@restatedev/restate-sdk';
import { serde, TerminalError } from '@restatedev/restate-sdk';
import { millisOrDurationToMillis, type Serde } from '@restatedev/restate-sdk-core';
import { open, type Database, type SqlValue, type Store, type TransactionHandle } from 'sqlite-kv';
import { sendClients } from './sends.ts';

export type { SqlValue };

/** SQL: the object's database, or one transaction of it. */
export interface SqlDatabase {
  /** Run SQL and return its rows; parameters bind to a single statement. */
  query(sql: string, params?: SqlValue[]): Promise<SqlValue[][]>;
  /** {@link query}, with the column names of the statement whose rows these are. */
  queryColumns(sql: string, params?: SqlValue[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  /** Run SQL (one or more statements), discarding rows. */
  execute(sql: string, params?: SqlValue[]): Promise<void>;
}

/**
 * The object's database, `ctx.db`, for the whole handler: each statement
 * outside {@link transaction} is its own transaction. Calls run one at a
 * time, in call order.
 */
export interface SqlSession extends SqlDatabase {
  /**
   * Run `body` as one SQL transaction: it commits when `body` resolves, and
   * rolls back when it rejects. The transaction holds the session: other
   * calls to the session wait until it ends, and a call from inside `body` is
   * refused, as it would wait for `body`. So `body` uses the `db` it gets,
   * which is invalid once `body` settles.
   */
  transaction<T>(body: (db: SqlDatabase) => Promise<T>): Promise<T>;
}

/**
 * The sends of a transaction: they are made once it commits, and only then.
 * They have no invocation id.
 */
export type SqlTransactionSends = Pick<
  restate.Context,
  'serviceSendClient' | 'objectSendClient' | 'workflowSendClient' | 'sendClient' | 'genericSend'
>;

/** What the body of `ctx.transaction` gets. */
export interface SqlTransaction {
  /** The transaction's SQL. */
  db: SqlDatabase;
  /** The transaction's sends. */
  tx: SqlTransactionSends;
}

/** What the session needs of the handler's context. */
export interface SqlHost {
  /** A shared handler's: it reads beside the exclusive one, and stores nothing. */
  readonly shared: boolean;
  readonly key: string;
  /** Rejects when the attempt ends (suspended, or aborted). */
  readonly invocationEndPromise: { readonly promise: Promise<unknown> };
  // oxlint-disable-next-line typescript/no-explicit-any
  readonly defaultSerde: Serde<any>;
  request(): { readonly target: { readonly service: string } };
  get(name: string, serde: Serde<Uint8Array>, options: restate.EntryOptions): PromiseLike<Uint8Array | null>;
  set(name: string, value: Uint8Array, serde: Serde<Uint8Array>, options: restate.EntryOptions): void;
  clear(name: string, options: restate.EntryOptions): void;
  run(name: string, action: () => void): PromiseLike<void>;
  genericSend(send: restate.GenericSend<Uint8Array>): unknown;
}

/** A store, as the sqlite-kv addon takes it. */
export interface SqlStore extends Store {
  get(key: string): Promise<Uint8Array | null | undefined>;
  sync(): Promise<void>;
}

type Wrap = (store: SqlStore) => SqlStore;
let wrapStore: Wrap | undefined;

/** Wrap each object's store, such as for fault injection in tests. */
export function setStoreWrapper(wrap: Wrap) {
  wrapStore = wrap;
}

/** The end of an attempt, as `invocationEndPromise` rejects with it, for the requests that would otherwise wait for it. */
export class AttemptEnd {
  #ended: Error | undefined;
  readonly #waiting = new Set<(error: Error) => void>();
  readonly #listeners = new Set<() => void>();

  constructor(end: Promise<unknown>) {
    end.catch((error: unknown) => {
      const ended = error instanceof Error ? error : new Error(String(error));
      this.#ended = ended;
      for (const reject of this.#waiting) reject(ended);
      this.#waiting.clear();
      for (const listener of this.#listeners) listener();
      this.#listeners.clear();
    });
  }

  /**
   * `promise`, or a rejection once the attempt ends: an attempt that is
   * suspended or aborted never settles what it was waiting for.
   */
  race<T>(promise: PromiseLike<T>): Promise<T> {
    if (this.#ended) return Promise.reject(this.#ended);
    return new Promise<T>((resolve, reject) => {
      this.#waiting.add(reject);
      void Promise.resolve(promise)
        .finally(() => this.#waiting.delete(reject))
        .then(resolve, reject);
    });
  }

  /** Call `listener` when the attempt ends; the returned function cancels it. */
  onEnd(listener: () => void): () => void {
    if (this.#ended) {
      listener();
      return () => {};
    }
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }
}

// A state command is named after its key, so that tooling can hide the
// database's pages and log by name (`dev.restate.tooling.journal.hidden.name.prefix`).
const named = (key: string): restate.EntryOptions => ({ name: key });

/**
 * The object's state as an ordered store: `get` is a state get, `set` and
 * `clear` are state writes, which return at once and which the runtime
 * stores in order; `sync` is an empty `ctx.run`, acknowledged after the
 * commands before it. A `get` or `sync` the attempt's end leaves waiting
 * rejects, so the session that made it ends instead of holding its lock for
 * the life of the process.
 */
// A shared handler's reader writes nothing.
const refusedWrite = () => {
  throw new Error('a shared handler writes no state');
};

function objectStore(ctx: SqlHost, end: AttemptEnd): SqlStore {
  // TODO: read with protocol v8's ephemeral get, left out of the journal, once
  // a Restate release ships v8 and the SDK lets a library make one (FUTURE.md
  // at the repository's root). Restate 1.8 negotiates v7 at most: a journaled get.
  const get = async (key: string) => (await end.race(ctx.get(key, serde.binary, named(key)))) ?? undefined;
  if (ctx.shared) return { get, set: refusedWrite, clear: refusedWrite, sync: async () => {} };
  return {
    get,
    set: (key, value) => ctx.set(key, value, serde.binary, named(key)),
    clear: (key) => ctx.clear(key, named(key)),
    sync: async () => {
      await end.race(ctx.run('sync', () => {}));
    },
  };
}

/**
 * The errors of a session's database calls: an SQL error, which a retry would
 * repeat, rejects as a {@link TerminalError}, unless the session has ended (a
 * storage error, which is retried).
 */
export class SqlErrors {
  readonly #errors = new WeakSet<object>();
  readonly #usable: () => boolean;

  /** `usable`: whether the session can still run SQL. */
  constructor(usable: () => boolean) {
    this.#usable = usable;
  }

  /** `promise`, its rejection the database's. */
  track<T>(promise: Promise<T>): Promise<T> {
    return promise.catch((error: unknown) => {
      throw this.#record(error);
    });
  }

  #record(error: unknown): unknown {
    if (typeof error === 'object' && error !== null) this.#errors.add(error);
    return this.classify(error, this.#usable());
  }

  /** `db`'s calls, their errors recorded. */
  view(db: SqlDatabase): SqlDatabase {
    return {
      query: (sql, params) => this.track(db.query(sql, params)),
      queryColumns: (sql, params) => this.track(db.queryColumns(sql, params)),
      execute: (sql, params) => this.track(db.execute(sql, params)),
    };
  }

  /**
   * `inner` as one transaction of `db`: the body's own error passes as it is,
   * the transaction's (a failed COMMIT) is recorded.
   */
  transaction<T>(db: Database, inner: (tx: SqlDatabase, handle: TransactionHandle) => Promise<T>): Promise<T> {
    let own: { error: unknown } | undefined;
    return db
      .transaction(async (tx) => {
        try {
          return await inner(this.view(tx), tx);
        } catch (error) {
          own = { error };
          throw error;
        }
      })
      .catch((error: unknown) => {
        throw own !== undefined && own.error === error ? error : this.#record(error);
      });
  }

  /**
   * What `error` fails with: an SQL error of the database's becomes terminal,
   * since a retry would repeat it. Anything else is as for any handler: a
   * {@link TerminalError} is terminal, and other errors (a storage error,
   * which ends the session, or the body's own, such as a failed request to
   * another service) are retried.
   */
  classify(error: unknown, usable: boolean): unknown {
    if (usable && error instanceof Error && !(error instanceof TerminalError) && this.#errors.has(error)) {
      const terminal = new TerminalError(error.message);
      terminal.cause = error;
      return terminal;
    }
    return error;
  }
}

// A transaction's sends commit with it, in the database's commit record (its
// effects): the outbox, which holds each committed transaction's sends until
// they are stored. They are made once the transaction commits; a later open
// makes again those still in the outbox, with the same idempotency keys
// (`transaction/<id>/<index>`, with a random id of the transaction), so
// making them again is dropped. Restate keeps an attempt's commands in the
// order they were made, so a send made before a commit is stored whenever the
// commit is: the next commit leaves the sends this attempt made out of the
// outbox, and so does the end of the attempt.

/** A send, but its parameter, as it is stored. */
type StoredSend = Omit<restate.GenericSend<unknown>, 'parameter' | 'inputSerde' | 'delay'> & { delay?: number };

/** The sends of a committed transaction, in the outbox. */
interface Outgoing {
  /** The transaction's random id. */
  id: string;
  /** Each send, its parameter in base64. */
  sends: (StoredSend & { parameter: string })[];
}

/** The outbox as effects: none is empty, as the record's effects start. */
function encodeOutbox(outbox: Outgoing[]): Uint8Array {
  return outbox.length === 0 ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(outbox));
}

function decodeOutbox(effects: Uint8Array): Outgoing[] {
  return effects.length === 0 ? [] : (JSON.parse(new TextDecoder().decode(effects)) as Outgoing[]);
}

function storedSend(send: restate.GenericSend<unknown>): Outgoing['sends'][number] {
  const { parameter, inputSerde, delay, ...rest } = send;
  return {
    ...rest,
    ...(delay !== undefined ? { delay: millisOrDurationToMillis(delay) } : {}),
    parameter: Buffer.from((inputSerde ?? serde.binary).serialize(parameter)).toString('base64'),
  };
}

/** How a handler ended. */
export type Outcome<T> = { value: T } | { error: unknown };

function settle<T>(outcome: Outcome<T>): T {
  if ('error' in outcome) throw outcome.error;
  return outcome.value;
}

/**
 * An attempt's SQL session: opened on its first use (`ctx.db` or
 * `ctx.transaction`), shared by everything the handler does, and saved when
 * the handler ends ({@link end}).
 */
export class SqlAttempt {
  readonly #ctx: SqlHost;
  readonly #errors = new SqlErrors(() => this.#db?.isUsable() ?? false);
  #session: Promise<Database> | undefined;
  // The session, once open.
  #db: Database | undefined;
  // The ids of the outbox's transactions this attempt made the sends of.
  readonly #made = new Set<string>();
  #ended = false;
  #cancel: () => void = () => {};
  /** `ctx.db`. */
  readonly db: SqlSession;

  constructor(ctx: SqlHost) {
    this.#ctx = ctx;
    this.db = {
      query: (sql, params) => this.#open().then((db) => this.#errors.track(db.query(sql, params))),
      queryColumns: (sql, params) => this.#open().then((db) => this.#errors.track(db.queryColumns(sql, params))),
      execute: (sql, params) => this.#open().then((db) => this.#errors.track(db.execute(sql, params))),
      transaction: (inner) => this.#open().then((db) => this.#errors.transaction(db, inner)),
    };
  }

  /**
   * `ctx.transaction(body)`: one transaction of the session, whose sends are
   * made once it commits, and kept in the outbox until they are stored.
   */
  async transaction<T>(body: (t: SqlTransaction) => Promise<T>): Promise<T> {
    if (this.#ctx.shared) throw new TerminalError('ctx.transaction is for exclusive handlers');
    const db = await this.#open();
    const sends: Outgoing['sends'] = [];
    let recording = true;
    const tx = sendClients(this.#ctx.defaultSerde, (send) => {
      if (!recording) throw new TerminalError('the transaction has ended: send from inside its body');
      sends.push(storedSend(send));
    });
    let outgoing: Outgoing | undefined;
    let value: T;
    try {
      value = await this.#errors.transaction(db, async (sql, handle) => {
        const result = await body({
          db: {
            ...sql,
            transaction: () => Promise.reject(new TerminalError('ctx.transaction already runs in a transaction')),
          } as SqlDatabase,
          tx,
        });
        recording = false;
        // The outbox as committed, but the sends stored since, and this
        // transaction's: unchanged, it is not written again.
        const outbox = await this.#unstored(db);
        if (sends.length > 0) {
          outgoing = { id: randomUUID(), sends };
          outbox.push(outgoing);
        }
        handle.commitEffects(encodeOutbox(outbox));
        return result;
      });
    } catch (error) {
      throw this.#errors.classify(error, db.isUsable());
    } finally {
      recording = false;
    }
    if (outgoing !== undefined) this.#make(outgoing);
    return value;
  }

  /** Make the sends of a committed transaction: each with an idempotency key of the transaction (or its own), so that making them again is dropped. */
  #make({ id, sends }: Outgoing) {
    sends.forEach(({ parameter, ...send }, index) => {
      this.#ctx.genericSend({
        ...send,
        parameter: new Uint8Array(Buffer.from(parameter, 'base64')),
        inputSerde: serde.binary,
        idempotencyKey: send.idempotencyKey ?? `transaction/${id}/${index}`,
      });
    });
    this.#made.add(id);
  }

  /** The outbox as committed, but the transactions this attempt made the sends of: a commit made after them stores them first. */
  async #unstored(db: Database): Promise<Outgoing[]> {
    return decodeOutbox(await db.effects()).filter(({ id }) => !this.#made.has(id));
  }

  /** Empty the outbox, so that a later open does not make its sends again: a commit after them, which stores them first. */
  async #flush(db: Database) {
    if ((await db.effects()).length === 0) return;
    const outbox = encodeOutbox(await this.#unstored(db));
    await db.transaction(async (tx) => tx.commitEffects(outbox));
  }

  #open(): Promise<Database> {
    if (this.#ended) return Promise.reject(new Error('the handler has ended: its SQL session is closed'));
    this.#session ??= this.#start();
    return this.#session;
  }

  async #start(): Promise<Database> {
    const ctx = this.#ctx;
    const namespace = `${ctx.request().target.service}/${ctx.key}`;
    const end = new AttemptEnd(ctx.invocationEndPromise.promise);
    const object = objectStore(ctx, end);
    const db = await open(namespace, wrapStore ? wrapStore(object) : object, { reader: ctx.shared });
    // A handler still waiting for something else when the attempt ends (a
    // model request, say) keeps the session no longer: it closes once no
    // statement runs, and the handler's later calls fail.
    this.#cancel = end.onEnd(() => void db.close().catch(() => {}));
    this.#db = db;
    if (ctx.shared) return db;
    try {
      // What a crash may have left unmade.
      decodeOutbox(await db.effects()).forEach((outgoing) => this.#make(outgoing));
    } catch (error) {
      await db.close();
      throw error;
    }
    return db;
  }

  /**
   * The handler ended with `outcome`: an SQL error that escaped it becomes
   * terminal, then the outbox is emptied, and the session is saved. One that
   * a storage error ended closes now, rather than when it is collected.
   */
  async end<T>(outcome: Outcome<T>): Promise<T> {
    this.#ended = true;
    const session = this.#session;
    if (session === undefined) return settle(outcome);
    let db: Database;
    try {
      db = await session;
    } catch {
      // The open failed, and its error went to the call that made it.
      return settle(outcome);
    }
    this.#cancel();
    if (this.#ctx.shared) {
      // What it read is the stored state only if no commit came since.
      const current = await db.isCurrent().catch(() => false);
      const usable = db.isUsable();
      await db.close();
      if (!current) throw new Error("the object's database changed while this shared handler read it: retrying");
      if ('error' in outcome) throw this.#errors.classify(outcome.error, usable);
      return outcome.value;
    }
    if ('error' in outcome) {
      const error = this.#errors.classify(outcome.error, db.isUsable());
      // The handler's error stands: what it committed is stored, and an
      // outbox left full is made again by the next open.
      const end = async () => {
        await this.#flush(db);
        await db.save();
      };
      await (db.isUsable() ? end() : db.close()).catch(() => db.close().catch(() => {}));
      throw error;
    }
    if (db.isUsable()) {
      await this.#flush(db);
      await db.save();
    } else {
      await db.close();
    }
    return outcome.value;
  }
}

/** Run `handler`, then end `attempt`'s SQL session, if it opened one (see {@link SqlAttempt.end}). */
export async function withSql<T>(handler: () => Promise<T>, attempt: SqlAttempt): Promise<T> {
  let outcome: Outcome<T>;
  try {
    outcome = { value: await handler() };
  } catch (error) {
    outcome = { error };
  }
  return attempt.end(outcome);
}

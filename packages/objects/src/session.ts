// An attempt's SQL session: the object's database, kept in its state
// (database.ts), opened on the handler's first use of `ctx.db` or
// `ctx.transaction` and saved when the handler ends; and
// `ctx.transaction(body)`, one transaction of it with the sends it commits
// with.
import { Buffer } from 'node:buffer';
import { randomUUID } from 'node:crypto';
import type * as restate from '@restatedev/restate-sdk';
import { serde, TerminalError } from '@restatedev/restate-sdk';
import { millisOrDurationToMillis, type Serde } from '@restatedev/restate-sdk-core';
import {
  open,
  type Row,
  sqlOver,
  SqlError,
  type Connection,
  type DatabaseWriter,
  type SqlParam,
  type DatabaseReader,
  type SqlValue,
  type Store,
  type ConnectionTransaction,
} from './database.ts';
import { apply, dropAll, ofObject, type Schema, sessions } from './schema.ts';
import { sendClients } from './sends.ts';
import { type Send, sendOf } from './calls.ts';

export type { DatabaseReader, DatabaseWriter, Row, SqlParam, SqlValue };

/**
 * The object's database, `ctx.db`, for the whole handler: each statement
 * outside {@link transaction} is its own transaction. Calls run one at a
 * time, in call order.
 */
export interface Database extends DatabaseWriter {
  /**
   * Run `body` as one transaction: it commits when `body` resolves, and rolls
   * back when it rejects. It holds the database: other calls wait until it
   * ends, and a call to the database from inside `body` is refused, as it
   * would wait for `body`. So `body` uses the `tx` it gets, which is invalid
   * once `body` settles.
   */
  transaction<T>(body: (tx: Transaction) => Promise<T>): Promise<T>;
}

/** One transaction of a {@link Database}: its SQL, and the sends it commits with. */
export interface Transaction extends DatabaseWriter {
  /**
   * Sends made right after the transaction commits, and never if it rolls
   * back. Each has the idempotency key it is given, or else one of the
   * transaction (`transaction/<id>/<index>`), so a send an attempt makes
   * again is dropped.
   */
  readonly send: Send;
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
  db: DatabaseWriter;
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
  request(): { readonly id: string; readonly target: { readonly service: string } };
  get(name: string, serde: Serde<Uint8Array>, options: restate.EntryOptions): PromiseLike<Uint8Array | null>;
  set(name: string, value: Uint8Array, serde: Serde<Uint8Array>, options: restate.EntryOptions): void;
  clear(name: string, options: restate.EntryOptions): void;
  /** The object's state keys. */
  stateKeys(): PromiseLike<string[]>;
  run(name: string, action: () => void): PromiseLike<void>;
  genericSend(send: restate.GenericSend<Uint8Array>): unknown;
}

type Wrap = (store: Store) => Store;
let wrapStore: Wrap | undefined;

/** Wrap each object's store, such as for fault injection in tests. */
export function setStoreWrapper(wrap: Wrap) {
  wrapStore = wrap;
}

/** The end of an attempt, as `invocationEndPromise` rejects with it, for the requests that would otherwise wait for it. */
class AttemptEnd {
  private ended: Error | undefined;
  /** The rejections of the requests waiting. */
  private readonly waiting = new Set<(error: Error) => void>();
  private readonly listeners = new Set<() => void>();

  constructor(end: Promise<unknown>) {
    end.catch((error: unknown) => {
      const ended = error instanceof Error ? error : new Error(String(error));
      this.ended = ended;
      for (const reject of this.waiting) reject(ended);
      this.waiting.clear();
      for (const listener of this.listeners) listener();
      this.listeners.clear();
    });
  }

  /**
   * `promise`, or a rejection once the attempt ends: an attempt that is
   * suspended or aborted never settles what it was waiting for.
   */
  race<T>(promise: PromiseLike<T>): Promise<T> {
    if (this.ended) return Promise.reject(this.ended);
    return new Promise<T>((resolve, reject) => {
      this.waiting.add(reject);
      void Promise.resolve(promise)
        .finally(() => this.waiting.delete(reject))
        .then(resolve, reject);
    });
  }

  /** Call `listener` when the attempt ends; the returned function cancels it. */
  onEnd(listener: () => void): () => void {
    if (this.ended) {
      listener();
      return () => {};
    }
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
}

// A state command is named after its key, so that tooling can hide the
// database's head, tail and packs by name (`dev.restate.tooling.journal.hidden.name.prefix`).
const named = (key: string): restate.EntryOptions => ({ name: key });

// A shared handler's reader writes nothing.
const refusedWrite = () => {
  throw new Error('a shared handler writes no state');
};

/**
 * The keys of the database `name`, apart from the object's own: each of the
 * format's keys (`v22/head`, `v22/tail/...`, `v22/manifest/...`,
 * `v22/pack/...`) as `v22/db/<name>/head`, under the format's prefix still,
 * which tooling hides, and under none of the object's database's.
 */
const beside = (name: string) => (key: string) => {
  const at = key.indexOf('/') + 1;
  return `${key.slice(0, at)}db/${name}/${key.slice(at)}`;
};

/**
 * The format's key of a key of the database `name` (undoes {@link beside});
 * `undefined` for any other key of the object.
 */
const unbeside = (name: string) => (key: string) => {
  const at = key.indexOf('/') + 1;
  const prefix = `db/${name}/`;
  return at > 0 && key.startsWith(prefix, at) ? key.slice(0, at) + key.slice(at + prefix.length) : undefined;
};

/**
 * The object's state as an ordered store: `get` is a state get, `set` and
 * `clear` are state writes, which return at once and which the runtime
 * stores in order; `sync` is an empty `ctx.run`, acknowledged after the
 * commands before it. A `get` or `sync` the attempt's end leaves waiting
 * rejects, so the session that made it ends instead of holding its lock for
 * the life of the process.
 */
function objectStore(ctx: SqlHost, end: AttemptEnd, dbName: string | undefined): Store {
  const keyOf = dbName === undefined ? (key: string) => key : beside(dbName);
  // TODO: read with protocol v8's ephemeral get, left out of the journal, once
  // a Restate release ships v8 and the SDK lets a library make one (FUTURE.md
  // at the repository's root). Restate 1.8 negotiates v7 at most: a journaled get.
  const get = async (key: string) =>
    (await end.race(ctx.get(keyOf(key), serde.binary, named(keyOf(key))))) ?? undefined;
  if (ctx.shared) return { get, set: refusedWrite, clear: refusedWrite, sync: async () => {} };
  return {
    get,
    set: (key, value) => ctx.set(keyOf(key), value, serde.binary, named(keyOf(key))),
    clear: (key) => ctx.clear(keyOf(key), named(keyOf(key))),
    // The database's keys, for a garbage collection.
    keys: async () => {
      const keys = await end.race(ctx.stateKeys());
      if (dbName === undefined) return keys;
      const own = unbeside(dbName);
      return keys.flatMap((key) => own(key) ?? []);
    },
    sync: async () => {
      await end.race(ctx.run('sync', () => {}));
    },
  };
}

/**
 * What a failed call to the session fails the handler with: an
 * {@link SqlError}, which a retry would repeat, becomes a
 * {@link TerminalError}, its `cause` the SQL error. Anything else is as for
 * any handler: a storage error, which ends the session, or the handler's own
 * (a failed request to another service, say) is retried, unless it is a
 * TerminalError itself.
 */
function handlerError(error: unknown): unknown {
  if (!(error instanceof SqlError)) return error;
  const terminal = new TerminalError(error.message);
  terminal.cause = error;
  return terminal;
}

/** `call`'s result, or the error the handler fails with (see {@link handlerError}). */
async function forHandler<T>(call: Promise<T>): Promise<T> {
  try {
    return await call;
  } catch (error) {
    throw handlerError(error);
  }
}

/** The SQL of a transaction's body: `tx`'s, failing as {@link forHandler} does. */
function forBody(tx: ConnectionTransaction): DatabaseWriter {
  return sqlOver((sql, params) => forHandler(tx.queryColumns(sql, params)));
}

/**
 * `connection`, opened without the SDK (`open`), as a handler's `ctx.db`:
 * for a library's tests, and tools. Its transactions send nothing, and a
 * library's schema applies to it (`Schema.ensure`).
 */
export function asDatabase(connection: Connection): Database {
  const db: Database = {
    ...sqlOver((sql, params) => connection.queryColumns(sql, params)),
    transaction: (body) => connection.transaction((tx) => body({ ...forBody(tx), send: refusedSend })),
  };
  sessions.set(db, (schemas) => apply(connection, schemas));
  return db;
}

// The transactions of `ctx.db`, each with how it empties the database (`reset`).
const resets = new WeakMap<Transaction, () => Promise<void>>();

/**
 * Empty the key's database in `tx`, a transaction of an exclusive handler's
 * `ctx.db`: every table, view, index and trigger is dropped, the schemas are
 * applied again (the object's, and the libraries' the attempt applied), and
 * the outbox stays. It commits with `tx`: what `tx` writes after it are the
 * new database's first rows, in the same commit.
 */
export function reset(tx: Transaction): Promise<void> {
  const of = resets.get(tx);
  if (of === undefined) throw new Error("reset takes a transaction of an exclusive handler's ctx.db");
  return forHandler(of());
}

/**
 * A database that `opener` opens on its first call, as a handler's `ctx.db`
 * opens: for tests, which give each attempt one over the same state.
 */
export function lazyDatabase(opener: () => Promise<Database>): Database {
  let opened: Promise<Database> | undefined;
  const db = () => (opened ??= opener());
  const lazy: Database = {
    query: async (sql, params) => (await db()).query(sql, params),
    get: async (sql, params) => (await db()).get(sql, params),
    values: async (sql, params) => (await db()).values(sql, params),
    execute: async (sql, params) => (await db()).execute(sql, params),
    transaction: async (body) => (await db()).transaction(body),
  };
  sessions.set(lazy, async (schemas) => {
    const inner = await db();
    const ensure = sessions.get(inner);
    if (ensure === undefined) throw new Error("lazyDatabase opens a handler's ctx.db, or a database of asDatabase");
    await ensure(schemas);
  });
  return lazy;
}

/** The sends of a transaction that has no outbox: a shared handler's, or another database's. */
const refusedSend: Send = () => {
  throw new TerminalError("a send of a transaction is an exclusive handler's, in the object's own database");
};

// A transaction's sends commit with it, in the database's record `outbox`
// (beside its tables, in none of them), which holds each committed
// transaction's sends until they are stored. They are made once the
// transaction commits; a later open makes again those still in the outbox,
// with the same idempotency keys (`transaction/<id>/<index>`, with a random
// id of the transaction), so making them again is dropped. Restate keeps an attempt's commands in the
// order they were made, so a send made before a commit is stored whenever the
// commit is: the next commit leaves the sends this attempt made out of the
// outbox, and so does the end of the attempt.

/** A send, but its parameter, as it is stored. */
type StoredSend = Omit<restate.GenericSend<unknown>, 'parameter' | 'inputSerde' | 'delay'> & {
  /** Its delay, in milliseconds. */
  delay?: number;
  /** When a delayed send is due (epoch ms): one made again waits only what remains. */
  due?: number;
};

/** The sends of a committed transaction, in the outbox. */
interface Outgoing {
  /** The transaction's random id. */
  id: string;
  /** Each send, its parameter in base64. */
  sends: (StoredSend & { parameter: string })[];
}

/** The database's record of the outbox. */
const OUTBOX = 'outbox';

/** The outbox as its record: none is empty, as a record never set reads. */
function encodeOutbox(outbox: Outgoing[]): Uint8Array {
  return outbox.length === 0 ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(outbox));
}

function decodeOutbox(record: Uint8Array): Outgoing[] {
  return record.length === 0 ? [] : (JSON.parse(new TextDecoder().decode(record)) as Outgoing[]);
}

function storedSend(send: restate.GenericSend<unknown>): Outgoing['sends'][number] {
  const { parameter, inputSerde, delay, ...rest } = send;
  return {
    ...rest,
    ...(delay !== undefined
      ? { delay: millisOrDurationToMillis(delay), due: Date.now() + millisOrDurationToMillis(delay) }
      : {}),
    parameter: Buffer.from((inputSerde ?? serde.binary).serialize(parameter)).toString('base64'),
  };
}

/** How a handler ended. */
type Outcome<T> = { value: T } | { error: unknown };

function settle<T>(outcome: Outcome<T>): T {
  if ('error' in outcome) throw outcome.error;
  return outcome.value;
}

// The attempts, by their `ctx.db`: for `database`.
const attempts = new WeakMap<object, SqlAttempt>();

// A database's name (`database`).
const DATABASE_NAME = /^[\w.-]{1,64}$/;

/**
 * The database `name` of the key, apart from the object's own (`ctx.db`, of
 * the attempt whose session `db` is): for a library whose tables must be
 * alone in their database, such as OpenCode's. It opens on its first use,
 * and ends with the attempt, as `ctx.db` does. A name is 1 to 64 letters,
 * digits, `_`, `-` or `.`.
 */
export function database(db: Database, name: string): Database {
  if (!DATABASE_NAME.test(name)) throw new Error(`database: ${JSON.stringify(name)} is not a database name`);
  const attempt = attempts.get(db);
  if (attempt === undefined) throw new Error("database takes a handler's ctx.db");
  return attempt.database(name);
}

/**
 * An attempt's SQL session: opened on its first use (`ctx.db` or
 * `ctx.transaction`), shared by everything the handler does, and saved when
 * the handler ends ({@link end}). With `name`, of the key's database `name`
 * ({@link database}), which has neither the object's schema nor an outbox.
 */
export class SqlAttempt {
  /** `ctx.db`. */
  readonly db: Database;
  private readonly ctx: SqlHost;
  private readonly name: string | undefined;
  /** The key's other databases this attempt opened, by name. */
  private readonly databases = new Map<string, SqlAttempt>();
  /** The session, from its first use on. */
  private session: Promise<Connection> | undefined;
  /** The ids of the outbox's transactions this attempt made the sends of. */
  private readonly made = new Set<string>();
  /** The schemas this attempt's database has all the steps of. */
  private readonly ensured = new Map<string, Schema>();
  private ended = false;
  /** Opened when the attempt started ({@link openNow}), and no call has seen how it went yet. */
  private unseenOpen = false;
  /** Stops closing the session when the attempt ends. */
  private keepOpen: () => void = () => {};

  constructor(ctx: SqlHost, name?: string) {
    this.ctx = ctx;
    this.name = name;
    this.db = {
      ...sqlOver((sql, params) => this.call((db) => db.queryColumns(sql, params))),
      // A shared handler's writes stay in its session: it has no outbox.
      transaction: <T>(body: (tx: Transaction) => Promise<T>) =>
        this.ctx.shared || this.name !== undefined
          ? this.call((db) => db.transaction((tx) => body({ ...forBody(tx), send: refusedSend })))
          : this.withSends((tx, record, handle) => {
              const transaction: Transaction = { ...tx, send: sendOf(record) };
              resets.set(transaction, () => this.reset(handle));
              return body(transaction);
            }),
    };
    sessions.set(this.db, (schemas) => this.opened().then((db) => this.ensureSchemas(db, schemas)));
    attempts.set(this.db, this);
  }

  /** The key's database `name`, in this attempt: see {@link database}. */
  database(name: string): Database {
    if (this.ended) throw new Error('the handler has ended: its SQL session is closed');
    let other = this.databases.get(name);
    if (other === undefined) this.databases.set(name, (other = new SqlAttempt(this.ctx, name)));
    return other.db;
  }

  /**
   * `ctx.transaction(body)`: one transaction of the session, whose sends are
   * made once it commits, and kept in the outbox until they are stored. A
   * call to the session from inside `body` is refused (see
   * {@link Connection.transaction}): `body` uses the `db` it gets.
   */
  async transaction<T>(body: (t: SqlTransaction) => Promise<T>): Promise<T> {
    if (this.ctx.shared) throw new TerminalError('ctx.transaction is for exclusive handlers');
    return this.withSends((db, record) => body({ db, tx: sendClients(this.ctx.defaultSerde, record) }));
  }

  /**
   * One transaction of the session, whose sends are made once it commits,
   * and kept in the outbox until they are stored: `body` gets its SQL, and
   * `record`, which records a send.
   */
  private async withSends<T>(
    body: (
      db: DatabaseWriter,
      record: (send: restate.GenericSend<unknown>) => void,
      handle: ConnectionTransaction,
    ) => Promise<T>,
  ): Promise<T> {
    const sends: Outgoing['sends'] = [];
    let recording = true;
    const record = (send: restate.GenericSend<unknown>) => {
      if (!recording) throw new TerminalError('the transaction has ended: send from inside its body');
      sends.push(storedSend(send));
    };
    let outgoing: Outgoing | undefined;
    const value = await this.call((db) =>
      db.transaction(async (handle) => {
        let result: T;
        try {
          result = await body(forBody(handle), record, handle);
        } finally {
          recording = false;
        }
        // The outbox as committed, but the sends stored since, and this
        // transaction's: unchanged, it is not written again.
        const outbox = await this.unstored(handle);
        if (sends.length > 0) {
          outgoing = { id: randomUUID(), sends };
          outbox.push(outgoing);
        }
        handle.setRecord(OUTBOX, encodeOutbox(outbox));
        return result;
      }),
    );
    if (outgoing !== undefined) this.make(outgoing);
    return value;
  }

  /**
   * The handler ended with `outcome`: an SQL error that escaped it becomes
   * terminal, then the outbox is emptied, and the session is saved. One that
   * a storage error ended closes now, rather than when it is collected.
   */
  async end<T>(outcome: Outcome<T>): Promise<T> {
    // The key's other databases end first, as the handler left them.
    const databases = await Promise.allSettled([...this.databases.values()].map((d) => d.end({ value: undefined })));
    const value = await this.endSession(outcome);
    const failed = databases.find((d) => d.status === 'rejected');
    if (failed) throw failed.reason;
    return value;
  }

  /**
   * Open the session now, as the attempt starts, rather than on its first
   * call: the open makes the sends a crashed attempt committed and left
   * unmade, which must not wait for a handler that uses the database.
   */
  openNow() {
    if (this.ctx.shared || this.name !== undefined || this.session !== undefined) return;
    this.unseenOpen = true;
    this.opened().catch(() => {});
  }

  /** Empty the database in the transaction `handle` ({@link reset}), and apply the schemas this attempt applied again. */
  private async reset(handle: ConnectionTransaction) {
    const own = ofObject(this.ctx.request().target.service);
    const schemas = [...(own ? [own] : []), ...[...this.ensured.values()].filter((s) => s !== own)];
    await dropAll(handle, schemas);
  }

  /** `call` on the session, opening it on the first; its failure as the handler fails with it. */
  private async call<T>(call: (db: Connection) => Promise<T>): Promise<T> {
    this.unseenOpen = false;
    const db = await this.opened();
    return forHandler(call(db));
  }

  /** Apply the steps of `schemas` that the database lacks, each schema once per attempt. */
  private async ensureSchemas(db: Connection, schemas: readonly Schema[]) {
    const lacking = schemas.filter((s) => !this.ensured.has(s.name));
    if (lacking.length === 0) return;
    await forHandler(apply(db, lacking));
    for (const s of lacking) this.ensured.set(s.name, s);
  }

  /** Make the sends of a committed transaction: each with an idempotency key of the transaction (or its own), so that making them again is dropped. */
  private make({ id, sends }: Outgoing, again = false) {
    sends.forEach(({ parameter, due, delay, ...send }, index) => {
      this.ctx.genericSend({
        ...send,
        // Made again, a delayed send waits only what remains of its delay.
        ...(delay !== undefined ? { delay: again && due !== undefined ? Math.max(0, due - Date.now()) : delay } : {}),
        parameter: new Uint8Array(Buffer.from(parameter, 'base64')),
        inputSerde: serde.binary,
        idempotencyKey: send.idempotencyKey ?? `transaction/${id}/${index}`,
      });
    });
    this.made.add(id);
  }

  /** The outbox as committed, but the transactions this attempt made the sends of: a commit made after them stores them first. */
  private async unstored(db: Connection | ConnectionTransaction): Promise<Outgoing[]> {
    return decodeOutbox(await db.record(OUTBOX)).filter(({ id }) => !this.made.has(id));
  }

  /**
   * Empty the outbox, so that a later open does not make its sends again: a
   * commit after them, which stores them first.
   */
  private async flush(db: Connection) {
    if ((await db.record(OUTBOX)).length === 0) return;
    const outbox = encodeOutbox(await this.unstored(db));
    await db.transaction(async (tx) => tx.setRecord(OUTBOX, outbox));
  }

  /** The session, opening it on the first call. */
  private opened(): Promise<Connection> {
    if (this.ended) return Promise.reject(new Error('the handler has ended: its SQL session is closed'));
    this.session ??= this.start();
    return this.session;
  }

  private async start(): Promise<Connection> {
    const ctx = this.ctx;
    // The page cache's: the key escaped, so that no key reads as another's
    // database (a name has no `/`).
    const key = `${ctx.request().target.service}/${encodeURIComponent(ctx.key)}`;
    const namespace = this.name === undefined ? key : `${key}/${this.name}`;
    const end = new AttemptEnd(ctx.invocationEndPromise.promise);
    const object = objectStore(ctx, end, this.name);
    const db = await open(namespace, wrapStore ? wrapStore(object) : object, { reader: ctx.shared });
    // A handler still waiting for something else when the attempt ends (a
    // model request, say) keeps the session no longer: it closes once no
    // statement runs, and the handler's later calls fail.
    this.keepOpen = end.onEnd(() => void db.close().catch(() => {}));
    // The object's schema: in a shared handler's session too, whose writes are never stored, so a
    // key never written reads as empty tables.
    const own = this.name === undefined ? ofObject(ctx.request().target.service) : undefined;
    try {
      // What a crash may have left unmade.
      if (!ctx.shared && this.name === undefined) {
        decodeOutbox(await db.record(OUTBOX)).forEach((outgoing) => this.make(outgoing, true));
      }
      if (own) await this.ensureSchemas(db, [own]);
    } catch (error) {
      const failed = handlerError(error);
      await db.close();
      throw failed;
    }
    return db;
  }

  private async endSession<T>(outcome: Outcome<T>): Promise<T> {
    this.ended = true;
    const session = this.session;
    if (session === undefined) return settle(outcome);
    let db: Connection;
    try {
      db = await session;
    } catch (error) {
      // The open failed, and its error went to the call that made it; one
      // made as the attempt started that no call saw fails the attempt, as
      // its outbox is still to be made.
      if (this.unseenOpen) throw error;
      return settle(outcome);
    }
    this.keepOpen();
    if (this.ctx.shared) {
      // It read the snapshot it opened on, whatever committed since; one that
      // outlived 16 commits failed its read ("is gone"), and is retried.
      const failed = 'error' in outcome ? handlerError(outcome.error) : undefined;
      await db.close();
      if ('error' in outcome) throw failed;
      return outcome.value;
    }
    if ('error' in outcome) {
      const error = handlerError(outcome.error);
      // The handler's error stands: what it committed is stored, and an
      // outbox left full is made again by the next open.
      await this.save(db).catch(() => db.close().catch(() => {}));
      throw error;
    }
    // A transaction left open, say, which a retry would leave open again.
    await forHandler(this.save(db));
    return outcome.value;
  }

  /** Empty the outbox and save the session; one a storage error ended only closes. */
  private async save(db: Connection) {
    if (!db.isUsable()) return db.close();
    await this.flush(db);
    await db.save();
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

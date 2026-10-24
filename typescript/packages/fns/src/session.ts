// An attempt's SQL session: the object's database, kept in its state through
// the SQL addon, opened on the handler's first use of `ctx.db`,
// `ctx.transaction` or `ctx.step` and saved when the handler ends;
// `ctx.transaction(body)`, one transaction of it with the sends it commits
// with; and `ctx.step(name, body)`, one whose result the invocation keeps.
import { AsyncLocalStorage } from 'node:async_hooks';
import { Buffer } from 'node:buffer';
import { randomUUID } from 'node:crypto';
import type * as restate from '@restatedev/restate-sdk';
import { serde, TerminalError } from '@restatedev/restate-sdk';
import { millisOrDurationToMillis, type Serde } from '@restatedev/restate-sdk-core';
import { open, type Database, type SqlParam, type SqlValue, type Store, type TransactionHandle } from 'restate-sql';
import { apply, ofObject, type Schema, sessions } from './schema.ts';
import { sendClients } from './sends.ts';

export type { SqlParam, SqlValue };

/** SQL: the object's database, or one transaction of it. */
export interface SqlDatabase {
  /** Run SQL and return its rows; parameters bind to a single statement. */
  query(sql: string, params?: SqlParam[]): Promise<SqlValue[][]>;
  /** {@link query}, with the column names of the statement whose rows these are. */
  queryColumns(sql: string, params?: SqlParam[]): Promise<{ columns: string[]; rows: SqlValue[][] }>;
  /** Run SQL (one or more statements), discarding rows. */
  execute(sql: string, params?: SqlParam[]): Promise<void>;
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
  request(): { readonly id: string; readonly target: { readonly service: string } };
  get(name: string, serde: Serde<Uint8Array>, options: restate.EntryOptions): PromiseLike<Uint8Array | null>;
  set(name: string, value: Uint8Array, serde: Serde<Uint8Array>, options: restate.EntryOptions): void;
  clear(name: string, options: restate.EntryOptions): void;
  run(name: string, action: () => void): PromiseLike<void>;
  genericSend(send: restate.GenericSend<Uint8Array>): unknown;
}

/** A store, as the SQL addon takes it. */
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
class AttemptEnd {
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

// A shared handler's reader writes nothing.
const refusedWrite = () => {
  throw new Error('a shared handler writes no state');
};

/**
 * The keys of the database `name`, beside the object's own: each of the
 * format's keys (`v16/root`, `v16/blob/...`) as `v16/<name>/root`, under
 * the format's prefix still, which tooling hides.
 */
const beside = (name: string) => (key: string) => {
  const at = key.indexOf('/');
  return `${key.slice(0, at)}/${name}/${key.slice(at + 1)}`;
};

/**
 * The object's state as an ordered store: `get` is a state get, `set` and
 * `clear` are state writes, which return at once and which the runtime
 * stores in order; `sync` is an empty `ctx.run`, acknowledged after the
 * commands before it. A `get` or `sync` the attempt's end leaves waiting
 * rejects, so the session that made it ends instead of holding its lock for
 * the life of the process.
 */
function objectStore(ctx: SqlHost, end: AttemptEnd, database: string | undefined): SqlStore {
  const keyOf = database === undefined ? (key: string) => key : beside(database);
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
class SqlErrors {
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

// The steps of the invocation in flight: each step's result, by the
// invocation's id (the request id, the same on every attempt) and the step's
// name, committed with the step's transaction. The object runs one exclusive
// invocation at a time, so once one ends, every row belongs to an invocation
// that ended: the end of an invocation that ran steps, or of the next one that
// uses the database after a killed invocation left rows, drops the table. It
// exists only while an invocation that ran steps is in flight.

/** The table of steps. */
const STEPS = 'restate_steps';

/** A step's result as stored: JSON, or NULL for `undefined`. */
const encodeStep = (value: unknown): string | null => (value === undefined ? null : JSON.stringify(value));

const decodeStep = (value: SqlValue): unknown => (value === null ? undefined : JSON.parse(value as string));

/**
 * The attempt whose transaction or step body runs: a call to its session from
 * inside the body would wait for the transaction, which waits for the body.
 */
const inBody = new AsyncLocalStorage<SqlAttempt>();

/** Refused inside a body: what the body uses instead. */
const NESTED = 'inside a transaction, use the transaction handle its body gets';

/** How a handler ended. */
type Outcome<T> = { value: T } | { error: unknown };

function settle<T>(outcome: Outcome<T>): T {
  if ('error' in outcome) throw outcome.error;
  return outcome.value;
}

// The attempts, by their `ctx.db`: for `databaseOf`.
const attempts = new WeakMap<object, SqlAttempt>();

/**
 * The database `name` of the key, apart from the object's own (`ctx.db`, of
 * the attempt whose session `db` is): for a library whose tables must be
 * alone in their database, such as OpenCode's. It opens on its first use,
 * and ends with the attempt, as `ctx.db` does.
 */
export function databaseOf(db: SqlSession, name: string): SqlSession {
  const attempt = attempts.get(db);
  if (attempt === undefined) throw new Error("databaseOf takes a handler's ctx.db");
  return attempt.database(name);
}

/**
 * An attempt's SQL session: opened on its first use (`ctx.db` or
 * `ctx.transaction`), shared by everything the handler does, and saved when
 * the handler ends ({@link end}). With `name`, of the key's database `name`
 * ({@link databaseOf}), which has neither the object's schema nor an outbox.
 */
export class SqlAttempt {
  readonly #ctx: SqlHost;
  readonly #name: string | undefined;
  // The key's other databases this attempt opened, by name.
  readonly #databases = new Map<string, SqlAttempt>();
  readonly #errors = new SqlErrors(() => this.#db?.isUsable() ?? false);
  #session: Promise<Database> | undefined;
  // The session, once open.
  #db: Database | undefined;
  // The ids of the outbox's transactions this attempt made the sends of.
  readonly #made = new Set<string>();
  // The names of the steps this attempt runs or ran: a name is one step.
  readonly #steps = new Set<string>();
  // Whether the table of steps exists, once known.
  #stepsTable: boolean | undefined;
  // The schemas this attempt's database has all the steps of.
  readonly #ensured = new Set<string>();
  #ended = false;
  #cancel: () => void = () => {};
  /** `ctx.db`. */
  readonly db: SqlSession;

  constructor(ctx: SqlHost, name?: string) {
    this.#ctx = ctx;
    this.#name = name;
    this.db = {
      query: (sql, params) => this.#use().then((db) => this.#errors.track(db.query(sql, params))),
      queryColumns: (sql, params) => this.#use().then((db) => this.#errors.track(db.queryColumns(sql, params))),
      execute: (sql, params) => this.#use().then((db) => this.#errors.track(db.execute(sql, params))),
      transaction: (inner) => this.#use().then((db) => this.#errors.transaction(db, inner)),
    };
    sessions.set(this.db, (schemas) => this.#open().then((db) => this.#apply(db, schemas)));
    attempts.set(this.db, this);
  }

  /** The key's database `name`, in this attempt: see {@link databaseOf}. */
  database(name: string): SqlSession {
    if (this.#ended) throw new Error('the handler has ended: its SQL session is closed');
    let database = this.#databases.get(name);
    if (database === undefined) this.#databases.set(name, (database = new SqlAttempt(this.#ctx, name)));
    return database.db;
  }

  /** Apply the steps of `schemas` that the database lacks, each schema once per attempt. */
  async #apply(db: Database, schemas: readonly Schema[]) {
    const lacking = schemas.filter((s) => !this.#ensured.has(s.name));
    if (lacking.length === 0) return;
    await this.#errors.track(apply(db, lacking));
    for (const s of lacking) this.#ensured.add(s.name);
  }

  /**
   * `ctx.transaction(body)`: one transaction of the session, whose sends are
   * made once it commits, and kept in the outbox until they are stored.
   */
  async transaction<T>(body: (t: SqlTransaction) => Promise<T>): Promise<T> {
    if (this.#ctx.shared) throw new TerminalError('ctx.transaction is for exclusive handlers');
    return this.#commit(await this.#use(), body, { begin: 'now' });
  }

  /**
   * `ctx.step(name, body)`: `body` as a transaction ({@link transaction})
   * whose result commits with it, as the invocation's step `name`. A later
   * attempt of the invocation gets the result, without running `body` again.
   */
  async step<T>(name: string, body: (t: SqlTransaction) => Promise<T>): Promise<T> {
    if (this.#ctx.shared) throw new TerminalError('ctx.step is for exclusive handlers');
    if (this.#steps.has(name)) throw new TerminalError(`ctx.step: the invocation has a step '${name}' already`);
    this.#steps.add(name);
    try {
      const db = await this.#use();
      const invocation = this.#ctx.request().id;
      if (await this.#hasSteps(db)) {
        const [done] = await this.#errors.track(
          db.query(`SELECT value FROM ${STEPS} WHERE invocation = ? AND name = ?`, [invocation, name]),
        );
        if (done !== undefined) return decodeStep(done[0]!) as T;
      }
      return await this.#commit(db, body, {
        begin: 'at first statement',
        record: async (sql, value) => {
          if (!this.#stepsTable) {
            await sql.execute(
              `CREATE TABLE IF NOT EXISTS ${STEPS} (invocation TEXT NOT NULL, name TEXT NOT NULL, value TEXT, PRIMARY KEY (invocation, name))`,
            );
          }
          await sql.execute(`INSERT INTO ${STEPS} VALUES (?, ?, ?)`, [invocation, name, encodeStep(value)]);
        },
      });
    } catch (error) {
      // Not committed: the name is free for another try.
      this.#steps.delete(name);
      throw error;
    }
  }

  /**
   * `body` as one transaction of `db`, with `record`'s writes of its result,
   * and the sends it made in the outbox. It begins before the body runs, or
   * at the body's first statement (or once it resolves, if it runs none):
   * until then the session is free, and other calls go first.
   */
  async #commit<T>(
    db: Database,
    body: (t: SqlTransaction) => Promise<T>,
    {
      begin: when,
      record,
    }: { begin: 'now' | 'at first statement'; record?: (sql: SqlDatabase, value: T) => Promise<void> },
  ): Promise<T> {
    const sends: Outgoing['sends'] = [];
    let recording = true;
    const tx = sendClients(this.#ctx.defaultSerde, (send) => {
      if (!recording) throw new TerminalError('the transaction has ended: send from inside its body');
      sends.push(storedSend(send));
    });
    const outcome = Promise.withResolvers<Outcome<T>>();
    let outgoing: Outgoing | undefined;
    let begun: Promise<SqlDatabase> | undefined;
    let committed: Promise<T> | undefined;
    const begin = () =>
      (begun ??= new Promise<SqlDatabase>((resolve, reject) => {
        committed = this.#errors.transaction(db, async (sql, handle) => {
          resolve(sql);
          const value = settle(await outcome.promise);
          await record?.(sql, value);
          // The outbox as committed, but the sends stored since, and this
          // transaction's: unchanged, it is not written again.
          const outbox = await this.#unstored(db);
          if (sends.length > 0) {
            outgoing = { id: randomUUID(), sends };
            outbox.push(outgoing);
          }
          handle.commitEffects(encodeOutbox(outbox));
          return value;
        });
        committed.catch(reject);
      }));
    const sql: SqlDatabase & Pick<SqlSession, 'transaction'> = {
      query: (statement, params) => begin().then((t) => t.query(statement, params)),
      queryColumns: (statement, params) => begin().then((t) => t.queryColumns(statement, params)),
      execute: (statement, params) => begin().then((t) => t.execute(statement, params)),
      transaction: () => Promise.reject(new TerminalError('ctx.transaction already runs in a transaction')),
    };
    try {
      if (when === 'now') await begin();
      outcome.resolve({ value: await inBody.run(this, () => body({ db: sql, tx })) });
    } catch (error) {
      outcome.resolve({ error });
      // A body that ran no SQL has no transaction to roll back.
      if (begun === undefined) throw this.#errors.classify(error, db.isUsable());
    } finally {
      recording = false;
    }
    let value: T;
    try {
      await begin();
      value = await committed!;
    } catch (error) {
      throw this.#errors.classify(error, db.isUsable());
    }
    if (record !== undefined) this.#stepsTable = true;
    if (outgoing !== undefined) this.#make(outgoing);
    return value;
  }

  /** Whether the table of steps exists. */
  async #hasSteps(db: Database): Promise<boolean> {
    this.#stepsTable ??=
      (await this.#errors.track(db.query(`SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?`, [STEPS])))
        .length > 0;
    return this.#stepsTable;
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

  /**
   * Empty the outbox, so that a later open does not make its sends again: a
   * commit after them, which stores them first. Unless `keepSteps`, the same
   * commit drops the table of steps: the invocation ends.
   */
  async #flush(db: Database, keepSteps: boolean) {
    const sending = (await db.effects()).length > 0;
    const dropping = !keepSteps && (await this.#hasSteps(db));
    if (!sending && !dropping) return;
    const outbox = encodeOutbox(await this.#unstored(db));
    await db.transaction(async (tx) => {
      if (dropping) await tx.execute(`DROP TABLE ${STEPS}`);
      tx.commitEffects(outbox);
    });
    this.#stepsTable = false;
  }

  /** The session, for a call that is not from inside a body ({@link inBody}). */
  async #use(): Promise<Database> {
    if (inBody.getStore() === this) throw new TerminalError(NESTED);
    return this.#open();
  }

  #open(): Promise<Database> {
    if (this.#ended) return Promise.reject(new Error('the handler has ended: its SQL session is closed'));
    this.#session ??= this.#start();
    return this.#session;
  }

  async #start(): Promise<Database> {
    const ctx = this.#ctx;
    const key = `${ctx.request().target.service}/${ctx.key}`;
    const namespace = this.#name === undefined ? key : `${key}/${this.#name}`;
    const end = new AttemptEnd(ctx.invocationEndPromise.promise);
    const object = objectStore(ctx, end, this.#name);
    const db = await open(namespace, wrapStore ? wrapStore(object) : object, { reader: ctx.shared });
    // A handler still waiting for something else when the attempt ends (a
    // model request, say) keeps the session no longer: it closes once no
    // statement runs, and the handler's later calls fail.
    this.#cancel = end.onEnd(() => void db.close().catch(() => {}));
    this.#db = db;
    // The object's schema: in a shared handler's session too, whose writes are never stored, so a
    // key never written reads as empty tables.
    const own = this.#name === undefined ? ofObject(ctx.request().target.service) : undefined;
    try {
      // What a crash may have left unmade.
      if (!ctx.shared && this.#name === undefined) {
        decodeOutbox(await db.effects()).forEach((outgoing) => this.#make(outgoing));
      }
      if (own) await this.#apply(db, [own]);
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
    // The key's other databases end first, as the handler left them.
    const databases = await Promise.allSettled([...this.#databases.values()].map((d) => d.end({ value: undefined })));
    const value = await this.#end(outcome);
    const failed = databases.find((d) => d.status === 'rejected');
    if (failed) throw failed.reason;
    return value;
  }

  async #end<T>(outcome: Outcome<T>): Promise<T> {
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
      // It read one stored state throughout, whatever committed since.
      const usable = db.isUsable();
      await db.close();
      if ('error' in outcome) throw this.#errors.classify(outcome.error, usable);
      return outcome.value;
    }
    if ('error' in outcome) {
      const error = this.#errors.classify(outcome.error, db.isUsable());
      // The handler's error stands: what it committed is stored, and an
      // outbox left full is made again by the next open. Restate retries an
      // error that is not terminal: the invocation goes on, with its steps.
      await this.#save(db, !(error instanceof TerminalError)).catch(() => db.close().catch(() => {}));
      throw error;
    }
    await this.#save(db, false);
    return outcome.value;
  }

  /** Empty the outbox (and, unless `keepSteps`, drop the steps) and save the session; one a storage error ended only closes. */
  async #save(db: Database, keepSteps: boolean) {
    if (!db.isUsable()) return db.close();
    await this.#flush(db, keepSteps);
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

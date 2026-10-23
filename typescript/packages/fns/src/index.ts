// SQL in Restate virtual objects: each key of an object has a database of its
// own, kept in the object's state, `ctx.db`. It is the Restate SDK, with
// `object`, `handlers.object` and the object contexts of SQL:
//
//   import * as restate from 'restate-fns';
//
//   const notes = restate.object({
//     name: 'notes',
//     schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
//     handlers: {
//       add: async (ctx: restate.ObjectContext, note: string) => {
//         await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
//       },
//       list: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) =>
//         ctx.db.query('SELECT text FROM notes'),
//       ),
//     },
//   });
//
// See README.md.
import * as restate from '@restatedev/restate-sdk';
import { register, type Step } from './schema.ts';
import { hookSelector } from './selector.ts';
import { SqlAttempt, type SqlHost, type SqlSession, type SqlTransaction, withSql } from './session.ts';

// The SDK, but what this module declares in its place.
export * from '@restatedev/restate-sdk';
export type { SqlDatabase, SqlSession, SqlTransaction, SqlTransactionSends, SqlParam, SqlValue } from './session.ts';
export { Schema, type Step } from './schema.ts';

/** The SDK's context of an object's handler that replays its journal (`journalReplay: true`): no `ctx.db`. */
export type JournaledObjectContext = restate.ObjectContext;
/** The SDK's context of an object's shared handler that replays its journal (`journalReplay: true`): no `ctx.db`. */
export type JournaledObjectSharedContext = restate.ObjectSharedContext;

/**
 * The context of an object's handler with SQL: the object's context, with its
 * database. The handler runs without journal replay: a retried attempt runs
 * it again from the start, against the state as stored (at least once), and
 * its operations can be awaited concurrently (`Promise.all`, `Promise.race`,
 * several async functions). Awakeables need journal replay, so there are
 * none: wait in a handler with `journalReplay: true`.
 */
export interface ObjectContext extends Omit<restate.ObjectContext, 'awakeable'> {
  /**
   * The object's SQL database, for the whole handler: each statement is its
   * own transaction, and `db.transaction` groups several. Calls run one at a
   * time, in call order, and a transaction holds the database until it ends.
   * Hand it to a library that wants a database for the handler's work.
   *
   * An attempt has one session: it opens on the first call to `ctx.db` or
   * `ctx.transaction`, and is saved when the handler ends. A commit is
   * durable once its writes are stored, which Restate does in order: a lost
   * commit takes everything after it, including calls and the response.
   *
   * An SQL error rejects as a `TerminalError` (its `cause` the SQL error),
   * since a retry would repeat it; a storage error, which ends the session,
   * is retried. The handler's own errors are as in any handler.
   */
  readonly db: SqlSession;

  /**
   * Run `body` as one transaction of {@link db}, with the sends it makes:
   * `body` gets `{ db, tx }`, the transaction's SQL and its send clients. It
   * commits when `body` resolves, and rolls back when it rejects; its sends
   * are made once it commits, and only then. So a committed transaction's
   * sends are made, even if the attempt then crashes, and those of one rolled
   * back are not. A call, whose result the body would wait for, cannot be
   * part of it: make calls before it, and record their outcome in it.
   */
  transaction<T>(body: (t: SqlTransaction) => Promise<T>): Promise<T>;
}

/**
 * The context of an object's shared handler with SQL: it reads the object's
 * database while an exclusive handler runs (or not), as of its first call to
 * it. What it writes stays in its session, and is never stored. If an
 * exclusive handler commits while it reads, the attempt fails when it ends,
 * and Restate retries it: what it returns was the stored state.
 */
export interface ObjectSharedContext extends Omit<restate.ObjectSharedContext, 'awakeable'> {
  readonly db: SqlSession;
}

/**
 * The options `object` sets: lazy state, so an invocation does not start by
 * loading every page; and no journal kept after an invocation completes,
 * since it holds every page it read.
 */
export const sqlOptions = { enableLazyState: true, journalRetention: 0 } satisfies restate.ObjectOptions;

/**
 * The metadata `object` sets: Restate's UI hides the database from the
 * journal view (its commit record, log, pages and hint in the object's
 * state, `v15/...`; and the `sync` runs after its writes).
 */
export const sqlMetadata = {
  'dev.restate.tooling.journal.hidden.names': 'sync',
  'dev.restate.tooling.journal.hidden.name.prefix': 'v15/',
};

// Marks the handlers made here.
const SQL = Symbol('restate-fns handler');

type Fn<C> = (ctx: C, input: never) => Promise<unknown>;

/** The SDK's runtime context, as far as the session needs it beyond its public type. */
type Runtime = { invocationEndPromise: SqlHost['invocationEndPromise']; defaultSerde: SqlHost['defaultSerde'] };

function host(ctx: restate.ObjectContext | restate.ObjectSharedContext, shared: boolean): SqlHost {
  const runtime = ctx as unknown as Runtime;
  const exclusive = ctx as restate.ObjectContext;
  return {
    shared,
    key: ctx.key,
    invocationEndPromise: runtime.invocationEndPromise,
    defaultSerde: runtime.defaultSerde,
    request: () => ctx.request(),
    get: (name, serde, entry) => ctx.get(name, serde, entry),
    set: (name, value, serde, entry) => exclusive.set(name, value, serde, entry),
    clear: (name, entry) => exclusive.clear(name, entry),
    run: (name, action) => exclusive.run(name, action),
    genericSend: (send) => ctx.genericSend(send),
  };
}

const refusedAwakeable = () => {
  throw new restate.TerminalError(
    'awakeables need journal replay: wait in a handler with journalReplay: true, which has no ctx.db',
  );
};

/** `ctx`, with `db` and `transaction` of `attempt`: its other members are the context's own. */
function sqlContext<C extends object>(ctx: C, attempt: SqlAttempt): C {
  const transaction = <T>(body: (t: SqlTransaction) => Promise<T>) => attempt.transaction(body);
  return new Proxy(ctx, {
    get(target, prop) {
      if (prop === 'db') return attempt.db;
      if (prop === 'transaction') return transaction;
      if (prop === 'awakeable') return refusedAwakeable;
      const value: unknown = Reflect.get(target, prop, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
}

/** `fn`, run with the context of {@link sqlContext}, its session saved when it ends. */
function withDatabase(fn: Fn<never>, shared: boolean) {
  return (ctx: restate.ObjectContext | restate.ObjectSharedContext, input: never) => {
    // A context with a database already, such as a test's: the handler uses it.
    if ('db' in ctx) return fn(ctx as never, input);
    const sql = host(ctx, shared);
    // One loop drives the attempt's awaits (see selector.ts), until it ends.
    const unhook = hookSelector(ctx);
    sql.invocationEndPromise.promise.catch(unhook);
    const attempt = new SqlAttempt(sql);
    return withSql(() => fn(sqlContext(ctx, attempt) as never, input), attempt).finally(unhook);
  };
}

// The handler options the SQL handlers have: explicit recovery, which runs a
// retried attempt from the start, since a later attempt makes different reads
// than a replayed journal would have.
const recovery = () => restate.internal.explicitRecovery();

/**
 * The options of an object's handler: the SDK's, and `journalReplay`. With
 * `journalReplay: true` the handler is the SDK's own, which replays its
 * journal and has no `ctx.db` (its context is {@link JournaledObjectContext}),
 * such as one that waits on an awakeable.
 */
// oxlint-disable-next-line typescript/no-explicit-any
export type SqlHandlerOpts = restate.ObjectHandlerOpts<any, any> & { journalReplay?: boolean };

/** `fn` as a handler of `kind`, with `options`. */
function handler(kind: 'exclusive' | 'shared', optionsOrFn: SqlHandlerOpts | Fn<never>, fn?: Fn<never>) {
  const [{ journalReplay = false, ...opts }, body] =
    typeof optionsOrFn === 'function' ? [{}, optionsOrFn] : [optionsOrFn, fn!];
  const make = (o: typeof opts, f: Fn<never>) =>
    kind === 'shared'
      ? restate.handlers.object.shared(o, f as never)
      : restate.handlers.object.exclusive(o, f as never);
  if (journalReplay) return make(opts, body);
  return Object.defineProperty(make({ ...opts, ...recovery() }, withDatabase(body, kind === 'shared')), SQL, {
    value: true,
  });
}

/** An exclusive handler with SQL (see {@link ObjectContext}), with `options`; one with `journalReplay: true` is the SDK's. */
function exclusive<F extends Fn<ObjectContext>>(fn: F): F;
function exclusive<F extends Fn<JournaledObjectContext>>(options: SqlHandlerOpts & { journalReplay: true }, fn: F): F;
function exclusive<F extends Fn<ObjectContext>>(options: SqlHandlerOpts, fn: F): F;
function exclusive(optionsOrFn: SqlHandlerOpts | Fn<never>, fn?: Fn<never>) {
  return handler('exclusive', optionsOrFn, fn);
}

/** A shared handler with SQL (see {@link ObjectSharedContext}), with `options`; one with `journalReplay: true` is the SDK's. */
function shared<F extends Fn<ObjectSharedContext>>(fn: F): F;
function shared<F extends Fn<JournaledObjectSharedContext>>(
  options: SqlHandlerOpts & { journalReplay: true },
  fn: F,
): F;
function shared<F extends Fn<ObjectSharedContext>>(options: SqlHandlerOpts, fn: F): F;
function shared(optionsOrFn: SqlHandlerOpts | Fn<never>, fn?: Fn<never>) {
  return handler('shared', optionsOrFn, fn);
}

/**
 * The SDK's handler wrappers, but an object's: its `exclusive` and `shared`
 * handlers have SQL, unless their options say `journalReplay: true`.
 */
export const handlers = { ...restate.handlers, object: { exclusive, shared } };

/** Whether `value` is a handler already: one of {@link handlers}, or the SDK's. */
const isHandler = (value: object) =>
  SQL in value || Object.getOwnPropertySymbols(value).some((s) => s.description === 'Handler');

/**
 * A Restate virtual object whose handlers have SQL, `ctx.db`: a database per
 * key, stored with the object's state. Each function in `handlers` is an
 * exclusive handler with SQL; `handlers.object.shared` makes a shared one,
 * `handlers.object.exclusive` one with options, and either one with
 * `journalReplay: true` one that replays its journal, without `ctx.db`.
 *
 * `schema`: the steps that make each key's database, in order, applied once
 * per key where its first handler opens it (see schema.ts):
 * `schema: ['CREATE TABLE notes (text TEXT NOT NULL)']`.
 *
 * `options` and `metadata` add to (or replace) {@link sqlOptions} and
 * {@link sqlMetadata}, such as `options: { journalRetention: { hours: 1 } }`
 * to look at journals in the UI.
 */
export function object<P extends string, H extends Record<string, unknown>>(definition: {
  name: P;
  handlers: H;
  schema?: readonly Step[];
  description?: string;
  metadata?: Record<string, string>;
  options?: restate.ObjectOptions;
}): restate.VirtualObjectDefinition<P, H> {
  const { schema, ...rest } = definition;
  if (schema) register(definition.name, schema);
  // The handlers as the SDK takes them.
  const sdkHandlers: Record<string, (ctx: restate.ObjectContext, input?: unknown) => Promise<unknown>> = {};
  for (const [name, fn] of Object.entries(definition.handlers)) {
    if (typeof fn !== 'function') throw new TypeError(`${definition.name}.${name} is not a handler`);
    sdkHandlers[name] = (isHandler(fn) ? fn : exclusive(fn as Fn<ObjectContext>)) as never;
  }
  return restate.object({
    ...rest,
    handlers: sdkHandlers,
    metadata: { ...sqlMetadata, ...definition.metadata },
    options: { ...sqlOptions, ...definition.options },
  });
}

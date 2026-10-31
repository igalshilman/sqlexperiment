// Session objects: a Restate virtual object whose state is a SQL database per
// key. Its handlers run one at a time per key, its views beside them; each
// gets a context of its own, made of the key, the request, the database and
// calls, and nothing else of the SDK's context:
//
//   const notes = restate.sessionObject({
//     name: 'notes',
//     schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
//     handlers: {
//       add: async (ctx, text: string) => {
//         await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [text]);
//       },
//     },
//     views: {
//       list: async (ctx) => ctx.db.query<{ text: string }>('SELECT text FROM notes'),
//     },
//   });
import * as restate from '@restatedev/restate-sdk';
import type { VirtualObjectDefinition } from '@restatedev/restate-sdk-core';
import { type Rpc, rpcOf, type Send, sendOf } from './calls.ts';
import type { DatabaseReader } from './database.ts';
import { register, sessions, type Step } from './schema.ts';
import { hookSelector } from './selector.ts';
import { type Database, SqlAttempt, type SqlHost, withSql } from './session.ts';

/**
 * The context of a session object's handler. A handler runs in attempts:
 * Restate starts a new one after a crash, a retryable error or a
 * suspension, and each runs the handler from its first line, finding what
 * the attempts before it committed. Its operations can be awaited together
 * (`Promise.all`, `Promise.race`).
 */
export interface SessionContext {
  /** The object's key. */
  readonly key: string;
  /** The request: its id (the same on every attempt), headers, target, and the attempt's signal. */
  readonly request: restate.Request;
  /**
   * The key's database. Each statement outside a transaction is its own
   * commit, and `db.transaction` groups several; a commit is durable when
   * its promise resolves. Calls run one at a time, in call order.
   */
  readonly db: Database;
  /** Calls, typed (`ctx.rpc(object, key).handler(input)`) or literal. */
  readonly rpc: Rpc;
  /** Sends, made at once; a transaction's (`tx.send`) are made once it commits. */
  readonly send: Send;
}

/**
 * The context of a session object's view: it runs beside the handlers, and
 * reads the database as committed at its first read, whatever they commit
 * meanwhile. It writes nothing, and sends nothing.
 */
export interface ViewContext {
  readonly key: string;
  readonly request: restate.Request;
  readonly db: DatabaseReader;
  readonly rpc: Rpc;
}

/** A handler of `C`: its input, if any, annotated. */
// oxlint-disable-next-line typescript/no-explicit-any
type Fn<C> = (ctx: C, input: any) => Promise<unknown>;

/** The SDK's options of a handler (`input`, `output`, `description`, `ingressPrivate`, ...). */
// oxlint-disable-next-line typescript/no-explicit-any
export type HandlerOptions = restate.ObjectHandlerOpts<any, any>;

/** A handler or a view: a function, or one with options. */
export type Entry<C> = Fn<C> | ({ handler: Fn<C> } & HandlerOptions);

/** Each entry's function, as clients see it; none for entries left out (inferred as the constraint). */
type Functions<H> = string extends keyof H ? unknown : { [K in keyof H]: H[K] extends { handler: infer F } ? F : H[K] };

/**
 * The options every session object has: lazy state, so an invocation does
 * not start by loading every page.
 *
 * TODO: preload the head eagerly (`eagerStateKeysWhitelist`), see FUTURE.md.
 */
export const sqlOptions = { enableLazyState: true } satisfies restate.ObjectOptions;

/**
 * The metadata every session object has: Restate's UI hides the database
 * from the journal view (its head, tail, manifests and packs in the object's
 * state, `v22/...`; and the `sync` runs after its writes).
 */
export const sqlMetadata = {
  'dev.restate.tooling.journal.hidden.names': 'sync',
  'dev.restate.tooling.journal.hidden.name.prefix': 'v22/',
};

// The handler options of every handler: explicit recovery, which runs a
// retried attempt from the start, since a later attempt makes different
// reads than a replayed journal would have.
export const recovery = () => restate.internal.explicitRecovery();

/** The SDK's runtime context, as far as the session needs it beyond its public type. */
type Runtime = { invocationEndPromise: SqlHost['invocationEndPromise']; defaultSerde: SqlHost['defaultSerde'] };

/** What an attempt needs of the SDK's context: a test's fake has these. */
export type AttemptContext = Pick<
  restate.ObjectContext,
  'key' | 'request' | 'get' | 'set' | 'clear' | 'stateKeys' | 'run' | 'genericSend' | 'genericCall'
> &
  Runtime;

/** What an attempt's session needs of the SDK's context `ctx`. */
export function host(
  ctx: restate.ObjectContext | restate.ObjectSharedContext | AttemptContext,
  shared: boolean,
): SqlHost {
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
    stateKeys: () => ctx.stateKeys(),
    run: (name, action) => exclusive.run(name, action),
    genericSend: (send) => ctx.genericSend(send),
  };
}

/** The SDK's contexts behind the session objects' contexts, for restate-objects/internal. */
export const sdkContexts = new WeakMap<
  SessionContext | ViewContext,
  restate.ObjectContext | restate.ObjectSharedContext
>();

/** The reads of `db`, which apply a library's schema as `db` does (`Schema.ensure`). */
function readerOf(db: Database): DatabaseReader {
  const reader: DatabaseReader = {
    query: (sql, params) => db.query(sql, params),
    get: (sql, params) => db.get(sql, params),
    values: (sql, params) => db.values(sql, params),
  };
  const ensure = sessions.get(db);
  if (ensure) sessions.set(reader, ensure);
  return Object.freeze(reader);
}

/**
 * The context of a handler (or, with `view`, of a view) over the SDK's
 * context `ctx`, with the database `db`: what an attempt gets, and a test's
 * over a context of its own.
 */
export function contextOf(ctx: AttemptContext, db: Database, view: true): ViewContext;
export function contextOf(ctx: AttemptContext, db: Database, view?: false): SessionContext;
export function contextOf(ctx: AttemptContext, db: Database, view = false): SessionContext | ViewContext {
  const rpc = rpcOf(ctx);
  const context: SessionContext | ViewContext = view
    ? Object.freeze({ key: ctx.key, request: ctx.request(), db: readerOf(db), rpc })
    : Object.freeze({
        key: ctx.key,
        request: ctx.request(),
        db,
        rpc,
        send: sendOf((send) => void ctx.genericSend(send)),
      });
  sdkContexts.set(context, ctx as unknown as restate.ObjectContext);
  return context;
}

/** One attempt of `fn` over `ctx`: its own context and session, which ends with it. */
function attempt(fn: Fn<never>, view: boolean, ctx: AttemptContext, input: unknown): Promise<unknown> {
  const session = new SqlAttempt(host(ctx, view));
  session.openNow();
  const context = view ? contextOf(ctx, session.db, true) : contextOf(ctx, session.db);
  return withSql(() => fn(context as never, input as never), session);
}

/** `fn` as the SDK's handler: one loop drives each attempt's awaits (see selector.ts), until it ends. */
function wrap(fn: Fn<never>, view: boolean) {
  return (ctx: restate.ObjectContext | restate.ObjectSharedContext, input: never) => {
    const unhook = hookSelector(ctx);
    const runtime = ctx as unknown as Runtime;
    runtime.invocationEndPromise.promise.catch(unhook);
    return attempt(fn, view, ctx as unknown as AttemptContext, input).finally(unhook);
  };
}

/** Each session object's handlers and views, by name: for {@link invoke}. */
const entries = new WeakMap<object, Map<string, { fn: Fn<never>; view: boolean }>>();

/**
 * Run handler or view `name` of `definition` for one attempt over `ctx`, a
 * context of a test's own, without Restate: for tests.
 */
export function invoke(definition: { name: string }, name: string, ctx: AttemptContext, input?: unknown) {
  const entry = entries.get(definition)?.get(name);
  if (entry === undefined) throw new Error(`${definition.name}.${name} is no handler or view of a session object`);
  return attempt(entry.fn, entry.view, ctx, input);
}

/** Whether `value` is a handler of the SDK's already. */
const isSdkHandler = (value: object) =>
  Object.getOwnPropertySymbols(value).some(
    (s) => s.description === 'Handler' || s.description === 'restate-objects handler',
  );

/** The SDK's handler of entry `name` of `object`. */
function handlerOf(object: string, name: string, entry: unknown, view: boolean) {
  const [fn, options] =
    typeof entry === 'function'
      ? [entry, {}]
      : typeof entry === 'object' && entry !== null && typeof (entry as { handler?: unknown }).handler === 'function'
        ? (() => {
            const { handler, ...rest } = entry as { handler: Fn<never> } & HandlerOptions;
            return [handler, rest] as const;
          })()
        : [undefined, undefined];
  if (fn === undefined)
    throw new TypeError(`${object}.${name} is not a handler: a function, or { handler, ...options }`);
  if (isSdkHandler(fn)) {
    throw new TypeError(
      `${object}.${name} is an SDK handler: a session object's handlers are functions of its context. ` +
        'Put journaled work in a restate.object, service or workflow, and call it with ctx.rpc',
    );
  }
  const opts = { ...options, ...recovery() } as HandlerOptions;
  const made = wrap(fn as Fn<never>, view) as never;
  const handler = view ? restate.handlers.object.shared(opts, made) : restate.handlers.object.exclusive(opts, made);
  return { handler, entry: { fn: fn as Fn<never>, view } };
}

/**
 * A virtual object whose state is a SQL database per key.
 *
 * - `schema`: the steps that make each key's database, in order, applied
 *   once per key by the first handler that uses it. A step that ran never
 *   changes: a change is a step of its own.
 * - `handlers`: they run one at a time per key, each with a
 *   {@link SessionContext}.
 * - `views`: they run beside the handlers, each with a {@link ViewContext}:
 *   they read, and write nothing.
 *
 * Each is a function, or `{ handler, ...options }` with the SDK's handler
 * options. A name is a handler or a view, not both. `description`,
 * `metadata` and `options` are the SDK's object fields.
 */
export function sessionObject<
  P extends string,
  // No defaults: a default stops each function's `ctx` from being inferred.
  H extends Record<string, Entry<SessionContext>>,
  V extends Record<string, Entry<ViewContext>>,
>(definition: {
  name: P;
  schema?: readonly Step[];
  handlers?: H;
  views?: V;
  description?: string;
  metadata?: Record<string, string>;
  options?: restate.ObjectOptions;
}): VirtualObjectDefinition<P, Functions<H> & Functions<V>> {
  const { name, schema, handlers = {}, views = {}, description, metadata, options } = definition;
  const both = Object.keys(handlers).filter((key) => key in views);
  if (both.length > 0) throw new Error(`${name}: ${both.join(', ')} is a handler and a view: name one otherwise`);
  if (schema) register(name, schema);
  const sdkHandlers: Record<string, (ctx: restate.ObjectContext, input?: unknown) => Promise<unknown>> = {};
  const own = new Map<string, { fn: Fn<never>; view: boolean }>();
  for (const [view, map] of [
    [false, handlers],
    [true, views],
  ] as const) {
    for (const [key, value] of Object.entries(map)) {
      const { handler, entry } = handlerOf(name, key, value, view);
      sdkHandlers[key] = handler;
      own.set(key, entry);
    }
  }
  const made = restate.object({
    name,
    description,
    handlers: sdkHandlers,
    metadata: { ...sqlMetadata, ...metadata },
    options: { ...sqlOptions, ...options },
  });
  entries.set(made, own);
  return made;
}

/** The SDK's context behind a session object's context. */
function sdkOf(ctx: SessionContext | ViewContext) {
  const sdk = sdkContexts.get(ctx);
  if (sdk === undefined) throw new Error("this takes a session object's ctx");
  return restate.internal.asInternal(sdk);
}

/**
 * Resolves when Restate cancels the invocation of `ctx`, a handler's with
 * `explicitCancellation: true`, whose awaits are left as they are: for a
 * library that stops its own work then (a model's request, say).
 */
export const cancelled = async (ctx: SessionContext): Promise<void> => {
  await sdkOf(ctx).cancellation();
};

/** Cancel the calls the attempt of `ctx` made, as a cancelled invocation's would be. */
export const cancelCalls = async (ctx: SessionContext): Promise<void> => {
  await sdkOf(ctx).cancelPreviousCalls();
};

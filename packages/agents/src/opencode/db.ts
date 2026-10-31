// OpenCode on the object's database: `openOpenCode(db, fence, options)` boots
// OpenCode 2.0's embedded host (@opencode/sdk) with its database in a
// SQL session, `ctx.db`.
//
// The host is OpenCode's workerd profile, the one it boots in a Cloudflare
// Durable Object: no local filesystem, shell, terminal or version control,
// config given inline, plugins given in code. Only its database differs. The
// workerd profile reads a Durable Object's SQLite, whose `exec` is
// synchronous; here OpenCode gets an Effect SqlClient instead, which is
// asynchronous, through `Database.configuredClient` (OpenCode's), the seam OpenCode keeps
// for an injected client. Each transaction is an SQL transaction, and the
// session runs statements one at a time, as OpenCode's own clients do.
//
// `fence.closed` refuses every statement from then on: an attempt that is
// giving up stores nothing more, as Restate fences an attempt that a newer
// one replaced.
import { Context, Effect, Exit, Layer, Option, Scope } from 'effect';
import { Reactivity } from 'effect/reactivity';
import { SqlClient, Statement } from 'effect/sql';
import { SqlError, UnknownError } from 'effect/sql/SqlError';
import type { Connection } from 'effect/sql/SqlConnection';
import { Database as OpenCodeDatabase } from '@opencode/core/database/database';
import { SessionRestart } from '@opencode/core/session/execution/restart';
import { ServerWorkerd } from '@opencode/server/workerd';
import { fromPromise } from '@opencode/core/plugin/promise';
import { OpenCode } from '@opencode/sdk/effect';
import type { Plugin } from '@opencode/plugin';
import type { DatabaseWriter, Database, SqlValue } from 'restate-session-objects';

/** Closed: every later statement fails. */
export interface Fence {
  closed: boolean;
}

const error = (operation: string, cause: unknown) =>
  new SqlError({
    reason: new UnknownError({
      cause,
      message: cause instanceof Error ? cause.message : String(cause),
      operation,
    }),
  });

/** The connection inside a transaction, for statements of that transaction. */
const InTransaction = Context.Service<SqlClient.TransactionConnection, SqlClient.TransactionConnection.Service>(
  'restate-agents/OpenCodeTransaction',
);

/**
 * A connection whose statements go to `target`: the session, which runs one
 * call at a time and holds them while a transaction runs, or that
 * transaction's database. Each is refused once `fence` closes.
 */
function connection(target: DatabaseWriter, fence: Fence): Connection {
  // Effect binds what the compiler made of OpenCode's values: SQL values.
  const exec = <T>(read: (sql: string, params: SqlValue[]) => Promise<T>, sql: string, params: readonly unknown[]) =>
    Effect.tryPromise({
      try: async () => {
        if (fence.closed) throw new Error('the attempt is over');
        return read(sql, params as SqlValue[]);
      },
      catch: (cause) => error('execute', cause),
    });
  const run = (sql: string, params: readonly unknown[] = []) => exec((s, p) => target.query(s, p), sql, params);
  const runValues = (sql: string, params: readonly unknown[] = []) => exec((s, p) => target.values(s, p), sql, params);
  const transformed: Connection['execute'] = (sql, params, transformRows) =>
    transformRows ? Effect.map(run(sql, params), transformRows) : run(sql, params);
  return {
    execute: transformed,
    executeRaw: (sql, params) => run(sql, params),
    executeValues: (sql, params) => runValues(sql, params),
    executeValuesUnprepared: (sql, params) => runValues(sql, params),
    executeUnprepared: transformed,
    executeStream: () => {
      throw new Error('executeStream is not supported');
    },
  };
}

/** An Effect SqlClient over `db`, with its statements refused once `fence` closes. */
export function sqlClient(db: Database, fence: Fence): Layer.Layer<SqlClient.SqlClient> {
  // `effect` as one SQL transaction: it commits when `effect` succeeds
  // and rolls back when it fails. Its statements go to the transaction's
  // database.
  const withTransaction = <A, E, R>(effect: Effect.Effect<A, E, R>) =>
    Effect.withFiber<A, E | SqlError, R>((fiber) => {
      if (Option.isSome(Context.getOption(fiber.context, InTransaction))) {
        return Effect.fail(error('transaction', new Error('nested transactions are not supported')));
      }
      return Effect.callback<A, E | SqlError, R>((resume) => {
        const rollback = new Error('rollback');
        let interrupted = false;
        let resumed = false;
        // Settles when the transaction has committed, or rejects: with
        // `rollback` once `effect` failed, or with the error of BEGIN or COMMIT.
        const ended: Promise<void> = db.transaction(
          (tx) =>
            new Promise<void>((resolve, reject) => {
              if (interrupted) return reject(rollback);
              const inner = Effect.provideContext(
                effect,
                Context.add(fiber.context, InTransaction, [connection(tx, fence), 0]),
              );
              resumed = true;
              resume(
                // A finalizer, so that the body settles however `effect`
                // ends, an interruption included: otherwise the transaction
                // would hold the session for good.
                Effect.onExit(inner, (exit) => {
                  if (Exit.isSuccess(exit)) {
                    resolve();
                    return Effect.void;
                  }
                  reject(rollback);
                  return Effect.promise(() => settled);
                }).pipe(
                  // The commit is part of the transaction: if it fails, so
                  // does the transaction.
                  Effect.flatMap((value) =>
                    Effect.as(Effect.tryPromise({ try: () => ended, catch: (e) => error('commit', e) }), value),
                  ),
                ),
              );
            }),
        );
        const settled = ended.catch((cause: unknown) => {
          // BEGIN failed: `effect` never ran.
          if (!resumed && cause !== rollback) resume(Effect.fail(error('transaction', cause)));
        });
        return Effect.suspend(() => {
          interrupted = true;
          return Effect.promise(() => settled);
        });
      });
    });

  const make = Effect.gen(function* () {
    const client = yield* SqlClient.make({
      acquirer: Effect.succeed(connection(db, fence)),
      compiler: Statement.makeCompilerSqlite(),
      spanAttributes: [['db.system.name', 'sqlite']],
      transactionService: InTransaction,
    });
    return Object.assign(client, { withTransaction, transactionStatements: false });
  });
  return Layer.effect(SqlClient.SqlClient, make).pipe(Layer.provide(Reactivity.layer));
}

export interface OpenOptions {
  /** OpenCode plugins: models, tools, agents. */
  plugins?: Plugin.Plugin[];
  /** OpenCode's config (opencode.json), inline. */
  config?: Record<string, unknown>;
  /**
   * OpenCode's own filesystem, search and shell, on the local machine, for
   * its built-in tools (read, write, edit, bash, glob, grep...) in a
   * session's directory. Without it, the workerd profile has none.
   */
  local?: boolean;
  /**
   * Resume the turns a previous attempt left unfinished, as OpenCode does
   * when it boots (default true). A reader beside a running turn does not.
   */
  resume?: boolean;
}

/** A booted OpenCode: `call` runs one of `client`'s calls (which return Effects); `close` shuts it down. */
export interface Host {
  client: OpenCode.Interface;
  call<A, E, R>(effect: Effect.Effect<A, E, R>): Promise<A>;
  close(): Promise<void>;
}

/**
 * How OpenCode resumes the turns a previous attempt left unfinished when it
 * boots: each, however often it crashed (Restate's retry policy decides how
 * often an attempt is retried), or, without `resume`, none.
 */
const restarts = (resume: boolean) =>
  resume
    ? SessionRestart.layer({ maxAttempts: Infinity })
    : Layer.succeed(SessionRestart.Service, { resumeSuspendedSessions: Effect.void });

/**
 * Boot OpenCode with its database in `db`. Booting resumes the turns a
 * previous attempt left unfinished. Close the host when done.
 */
export async function openOpenCode(
  db: Database,
  fence: Fence,
  { plugins = [], config = {}, local = false, resume = true }: OpenOptions,
): Promise<Host> {
  // The workerd profile, but for its database, which comes first. (The
  // Promise API's OpenCode.create takes no replacements; the Effect API's does.)
  const profile = { storage: undefined as never, models: { fetch: false } };
  // In 2.0.27's order: database, process spawner, snapshot, version control,
  // filesystem, filesystem search, pty, plugin discovery. Local keeps the
  // stubs of snapshots, the pty and plugin discovery only.
  const [, spawner, snapshot, vcs, fs, search, pty, discovery] = ServerWorkerd.replacements(profile);
  const rest = local ? [snapshot, pty, discovery] : [spawner, snapshot, vcs, fs, search, pty, discovery];
  const scope = Scope.makeUnsafe();
  const client = await Effect.runPromise(
    OpenCode.create(
      {
        ...ServerWorkerd.serverOptions({ ...profile, config: { content: JSON.stringify(config) } }),
        plugins: plugins.map(fromPromise),
      },
      {
        overrides: [
          OpenCodeDatabase.node.replace(OpenCodeDatabase.configuredClient(sqlClient(db, fence))),
          ...rest,
          SessionRestart.node.replace(SessionRestart.node.mapLayer(() => restarts(resume) as never)),
        ],
      },
    ).pipe(Scope.provide(scope)),
  );
  return {
    client,
    // The client's calls need nothing more: OpenCode provided it all.
    call: <A, E, R>(effect: Effect.Effect<A, E, R>) => Effect.runPromise(effect as Effect.Effect<A, E>),
    close: () => Effect.runPromise(Scope.close(scope, Exit.void)),
  };
}

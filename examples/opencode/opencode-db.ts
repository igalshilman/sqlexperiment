// OpenCode on the object's database: `openOpenCode(db, options)` boots
// OpenCode 2.0's embedded host (@opencode/sdk) with its database in a
// sqlite-kv session, `ctx.db`.
//
// The host is OpenCode's workerd profile, the one it boots in a Cloudflare
// Durable Object: no local filesystem, shell, terminal or version control,
// config given inline, plugins given in code. Only its database differs. The
// workerd profile reads a Durable Object's SQLite, whose `exec` is
// synchronous; here OpenCode gets an Effect SqlClient instead, which is
// asynchronous, through `Database.configuredClient`, the seam OpenCode keeps
// for an injected client. Each transaction is a sqlite-kv transaction, and the
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
import { Database } from '@opencode/core/database/database';
import { SessionRestart } from '@opencode/core/session/execution/restart';
import { ServerWorkerd } from '@opencode/server/workerd';
import { fromPromise } from '@opencode/core/plugin/promise';
import { OpenCode } from '@opencode/sdk/effect';
import type { Plugin } from '@opencode/plugin';
import type { SqlDatabase, SqlSession, SqlValue } from '@restatedev/restate-sdk';

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
  'sqlite-kv-examples/OpenCodeTransaction',
);

/** An Effect SqlClient over `db`, with its statements refused once `fence` closes. */
export function sqlClient(db: SqlSession, fence: Fence): Layer.Layer<SqlClient.SqlClient> {
  // Statements through `target`: the session, which runs one call at a time
  // and holds them while a transaction runs, or that transaction's database.
  const connectionOf = (target: SqlDatabase): Connection => {
    const exec = (sql: string, params: readonly unknown[]) =>
      Effect.tryPromise({
        try: async () => {
          if (fence.closed) throw new Error('the attempt is over');
          // Effect binds what the compiler made of OpenCode's values: SQL values.
          return target.queryColumns(
            sql,
            params.map((value) => value as SqlValue),
          );
        },
        catch: (cause) => error('execute', cause),
      });
    const run = (sql: string, params: readonly unknown[] = []) =>
      Effect.map(exec(sql, params), ({ columns, rows }) =>
        rows.map((row) => Object.fromEntries(columns.map((column, i) => [column, row[i]]))),
      );
    const runValues = (sql: string, params: readonly unknown[] = []) =>
      Effect.map(exec(sql, params), ({ rows }) => rows);
    return {
      execute: (sql, params, transformRows) =>
        transformRows ? Effect.map(run(sql, params), transformRows) : run(sql, params),
      executeRaw: (sql, params) => run(sql, params),
      executeValues: (sql, params) => runValues(sql, params),
      executeValuesUnprepared: (sql, params) => runValues(sql, params),
      executeUnprepared: (sql, params, transformRows) =>
        transformRows ? Effect.map(run(sql, params), transformRows) : run(sql, params),
      executeStream: () => {
        throw new Error('executeStream is not supported');
      },
    };
  };

  // `effect` as one sqlite-kv transaction: it commits when `effect` succeeds
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
                Context.add(fiber.context, InTransaction, [connectionOf(tx), 0]),
              );
              resumed = true;
              resume(
                Effect.flatMap(Effect.exit(inner), (exit) => {
                  if (Exit.isFailure(exit)) {
                    reject(rollback);
                    return Effect.flatMap(
                      Effect.promise(() => settled),
                      () => exit,
                    );
                  }
                  resolve();
                  // The commit is part of the transaction: if it fails, so
                  // does the transaction.
                  return Effect.flatMap(
                    Effect.tryPromise({ try: () => ended, catch: (e) => error('commit', e) }),
                    () => exit,
                  );
                }),
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
      acquirer: Effect.succeed(connectionOf(db)),
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
}

/** OpenCode's client, whose calls return Effects. */
export type Client = OpenCode.Interface;

/** A booted OpenCode: `call` runs one of `client`'s calls; `close` shuts it down. */
export interface Host {
  client: Client;
  call<A, E, R>(effect: Effect.Effect<A, E, R>): Promise<A>;
  close(): Promise<void>;
}

/**
 * Boot OpenCode with its database in `db`. Booting resumes the turns a
 * previous attempt left unfinished. Close the host when done.
 */
export async function openOpenCode(
  db: SqlSession,
  fence: Fence,
  { plugins = [], config = {} }: OpenOptions,
): Promise<Host> {
  // The workerd profile, but for its database, which comes first. (The
  // Promise API's OpenCode.create takes no replacements; the Effect API's does.)
  // OpenCode stops resuming a turn that crashed ten times, and fails it;
  // here Restate's retry policy decides how often an attempt is retried.
  const profile = { storage: undefined as never, models: { fetch: false } };
  const [, ...rest] = ServerWorkerd.replacements(profile);
  const scope = Scope.makeUnsafe();
  const client = await Effect.runPromise(
    OpenCode.create(
      {
        ...ServerWorkerd.serverOptions({ ...profile, config: { content: JSON.stringify(config) } }),
        plugins: plugins.map(fromPromise),
      },
      {
        overrides: [
          Database.node.replace(Database.configuredClient(sqlClient(db, fence))),
          ...rest,
          SessionRestart.node.replace(
            SessionRestart.node.mapLayer(() => SessionRestart.layer({ maxAttempts: Infinity }) as never),
          ),
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

// The Effect SqlClient of restate-agents/opencode/db over an SQL database in memory.
// `make test-ts T=opencode`.
import type { SqlSession } from 'restate-fns';
import { Effect, Exit, Fiber } from 'effect';
import { SqlClient } from 'effect/sql';
import { open } from 'restate-sql';
import { describe, expect, it } from 'vitest';
import { memoryStore } from '../testing.ts';
import { sqlClient } from 'restate-agents/opencode/db';

let n = 0;
async function run<A, E>(body: (sql: SqlClient.SqlClient) => Effect.Effect<A, E>) {
  const db = await open(`opencode-db-${process.pid}-${n++}`, memoryStore());
  const program = Effect.gen(function* () {
    return yield* body(yield* SqlClient.SqlClient);
  });
  return Effect.runPromise(program.pipe(Effect.provide(sqlClient(db as unknown as SqlSession, { closed: false }))));
}

describe('the OpenCode SqlClient', () => {
  it('fails a transaction whose commit fails, and keeps none of it', async () => {
    const [outcome, rows] = await run((sql) =>
      Effect.gen(function* () {
        yield* sql.unsafe('PRAGMA foreign_keys=ON');
        yield* sql.unsafe('CREATE TABLE p(id INTEGER PRIMARY KEY)');
        yield* sql.unsafe('CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)');
        // The deferred constraint fails at COMMIT, not at the INSERT.
        const exit = yield* Effect.exit(sql.withTransaction(sql.unsafe('INSERT INTO c VALUES(7)')));
        const left = yield* sql.unsafe('SELECT * FROM c');
        return [exit, left] as const;
      }),
    );
    expect(Exit.isFailure(outcome)).toBe(true);
    expect(rows).toEqual([]);
  });

  it('commits a transaction that succeeds', async () => {
    const rows = await run((sql) =>
      Effect.gen(function* () {
        yield* sql.unsafe('CREATE TABLE t(id INTEGER)');
        yield* sql.withTransaction(sql.unsafe('INSERT INTO t VALUES(1)'));
        return yield* sql.unsafe('SELECT id FROM t');
      }),
    );
    expect(rows).toEqual([{ id: 1 }]);
  });

  it('rolls back a transaction whose fiber is interrupted, and goes on', async () => {
    const rows = await run((sql) =>
      Effect.gen(function* () {
        yield* sql.unsafe('CREATE TABLE t(id INTEGER)');
        const fiber = yield* Effect.forkChild(
          sql.withTransaction(Effect.andThen(sql.unsafe('INSERT INTO t VALUES(1)'), Effect.sleep('1 second'))),
        );
        yield* Effect.sleep('20 millis');
        yield* Fiber.interrupt(fiber);
        // The session is not left held by the transaction, and none of it stays.
        yield* sql.withTransaction(sql.unsafe('INSERT INTO t VALUES(2)'));
        return yield* sql.unsafe('SELECT id FROM t');
      }),
    );
    expect(rows).toEqual([{ id: 2 }]);
  });
});

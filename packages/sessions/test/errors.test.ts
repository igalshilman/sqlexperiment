// Which errors fail a handler for good, as a TerminalError, and which Restate
// retries: an SQL error would repeat on a retry, so it is terminal while the
// session goes on; a storage error ends the session, and is retried; the
// handler's own errors pass as they are.
import * as restate from 'restate-sessions';
import { expect, it } from 'vitest';
import { sqlAttempt } from './support.ts';

/** What `promise` rejects with; undefined if it resolves. */
const failure = (promise: Promise<unknown>) =>
  promise.then(
    () => undefined,
    (error: unknown) => error,
  );

it('an SQL error of ctx.db is terminal, and the session goes on', async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    const error = await failure(a.db.execute('SELECT * FROM missing'));
    expect(error).toBeInstanceOf(restate.TerminalError);
    expect((error as Error).message).toMatch(/missing/);
    expect((error as Error).cause).toBeInstanceOf(Error);
    expect(await a.db.values('SELECT 1')).toEqual([[1]]);
  });
});

it("an SQL error inside a transaction is terminal there, and the body's rethrowing it rolls back", async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    await a.db.execute('CREATE TABLE t (n INTEGER)');
    let inside: unknown;
    const error = await failure(
      a.db.transaction(async (tx) => {
        await tx.execute('INSERT INTO t VALUES (1)');
        inside = await failure(tx.execute('INSERT INTO missing VALUES (1)'));
        throw inside;
      }),
    );
    expect(inside).toBeInstanceOf(restate.TerminalError);
    expect(error).toBe(inside);
    expect(await a.db.values('SELECT count(*) FROM t')).toEqual([[0]]);
  });
});

it("the body's own error passes as it is, and is retried", async () => {
  const a = sqlAttempt({ map: new Map() });
  const own = new Error('the payment service is down');
  const error = await failure(
    a.end(() =>
      a.db.transaction(async (tx) => {
        await tx.values('SELECT 1');
        throw own;
      }),
    ),
  );
  expect(error).toBe(own);
});

it('a call to ctx.db, or a nested transaction, from inside a body is terminal', async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    await a.db.execute('CREATE TABLE t (n INTEGER)');
    const errors = await Promise.all([
      failure(a.db.transaction(() => a.db.values('SELECT 1'))),
      failure(a.db.transaction(() => a.db.transaction(async () => {}))),
    ]);
    for (const error of errors) expect(error).toBeInstanceOf(restate.TerminalError);
    expect(await a.db.values('SELECT 1')).toEqual([[1]]);
  });
});

it('a parameter the session cannot bind is terminal', async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    const error = await failure(a.db.values('SELECT ?', [new Date() as never]));
    expect(error).toBeInstanceOf(restate.TerminalError);
    expect(await a.db.values('SELECT 1')).toEqual([[1]]);
  });
});

it('statements a body awaits together run in order, and none outlives its rollback', async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    await a.db.execute('CREATE TABLE t (n INTEGER)');
    await a.db.transaction((tx) => Promise.all([1, 2, 3].map((n) => tx.execute('INSERT INTO t VALUES (?)', [n]))));
    expect(await a.db.values('SELECT n FROM t ORDER BY rowid')).toEqual([[1], [2], [3]]);
    const error = await failure(
      a.db.transaction((tx) =>
        Promise.all([tx.execute('INSERT INTO missing VALUES (1)'), tx.execute('INSERT INTO t VALUES (4)')]),
      ),
    );
    expect(error).toBeInstanceOf(restate.TerminalError);
    expect(await a.db.values('SELECT count(*) FROM t')).toEqual([[3]]);
  });
});

it('statements a body makes but does not await run before its commit', async () => {
  const a = sqlAttempt({ map: new Map() });
  await a.end(async () => {
    await a.db.execute('CREATE TABLE t (n INTEGER)');
    await a.db.transaction(async (tx) => {
      void tx.execute('INSERT INTO t VALUES (1)');
      void tx.execute('INSERT INTO t VALUES (2)');
    });
    expect(await a.db.values('SELECT n FROM t ORDER BY n')).toEqual([[1], [2]]);
  });
});

it('a statement that ends the transaction of a body is terminal, though it ends the session', async () => {
  const a = sqlAttempt({ map: new Map() });
  const error = await failure(a.end(() => a.db.transaction((tx) => tx.execute('COMMIT'))));
  expect(error).toBeInstanceOf(restate.TerminalError);
  expect((error as Error).message).toMatch(/ended the transaction/);
});

it('a transaction the handler left open is terminal', async () => {
  const a = sqlAttempt({ map: new Map() });
  const error = await failure(a.end(() => a.db.execute('BEGIN')));
  expect(error).toBeInstanceOf(restate.TerminalError);
  expect((error as Error).message).toMatch(/left open/);
});

it('a storage error ends the session, and is retried', async () => {
  const fence = { closed: false };
  const a = sqlAttempt({ map: new Map(), fence });
  const error = await failure(
    a.end(async () => {
      await a.db.execute('CREATE TABLE t (n INTEGER)');
      fence.closed = true;
      await a.db.execute('INSERT INTO t VALUES (1)');
    }),
  );
  expect(error).toBeInstanceOf(Error);
  expect(error).not.toBeInstanceOf(restate.TerminalError);
  expect((error as Error).message).toMatch(/fenced/);
});

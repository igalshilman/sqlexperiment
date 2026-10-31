// A key's databases apart from the object's own (database of restate-objects),
// without a Restate server: each its own tables, stored in the object's state
// beside `ctx.db`'s, opened on first use and ended with the attempt.
import { randomUUID } from 'node:crypto';
import type { DatabaseWriter } from 'restate-objects';
import { database } from 'restate-objects/internal';
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from './support.ts';

/** Make table `t` in `db` and insert three rows, one statement at a time; then its rows. */
async function insertThree(db: DatabaseWriter) {
  await db.execute('CREATE TABLE t (x)');
  for (let i = 0; i < 3; i++) await db.execute('INSERT INTO t VALUES (1)');
  return db.values('SELECT count(*) FROM t');
}

describe("a key's other database", () => {
  it('keeps tables apart from the object’s own, through attempts', async () => {
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map });
    await first.end(async () => {
      await first.db.execute('CREATE TABLE notes (text TEXT)');
      const other = database(first.db, 'library');
      await other.execute('CREATE TABLE notes (text TEXT)');
      await other.execute("INSERT INTO notes VALUES ('its own')");
    });
    const later = sqlAttempt({ map, id: 'later' });
    const [yours, its] = await later.end(async () => [
      await later.db.values('SELECT text FROM notes'),
      await database(later.db, 'library').values('SELECT text FROM notes'),
    ]);
    expect(yours).toEqual([]);
    expect(its).toEqual([['its own']]);
  });

  it('is stored under the format’s prefix, apart from the object’s database', async () => {
    const map = new Map<string, Uint8Array>();
    const attempt = sqlAttempt({ map });
    await attempt.end(() => database(attempt.db, 'pack').execute('CREATE TABLE t (x)'));
    expect([...map.keys()].filter((key) => !key.startsWith('v22/db/pack/'))).toEqual([]);
    expect(map.has('v22/db/pack/head')).toBe(true);
  });

  it('has a name of letters, digits, _, - and .', () => {
    const attempt = sqlAttempt({ map: new Map() });
    for (const name of ['', 'a/b', 'a b', 'x'.repeat(65)]) {
      expect(() => database(attempt.db, name)).toThrow(/not a database name/);
    }
  });

  it('is apart from the database of a key that ends in its name', async () => {
    // Opened at once, each committing in turn: sharing a namespace, each
    // would refuse the other's flushes.
    const service = `test-${randomUUID()}`;
    const slashed = sqlAttempt({ map: new Map(), service, key: 'x/library' });
    const named = sqlAttempt({ map: new Map(), service, key: 'x' });
    const counts = await Promise.all([
      slashed.end(() => insertThree(slashed.db)),
      named.end(() => insertThree(database(named.db, 'library'))),
    ]);
    expect(counts).toEqual([[[3]], [[3]]]);
  });

  it('reads in a shared handler, storing nothing', async () => {
    const map = new Map<string, Uint8Array>();
    const writer = sqlAttempt({ map });
    await writer.end(async () => {
      const other = database(writer.db, 'library');
      await other.execute('CREATE TABLE t (x)');
      await other.execute('INSERT INTO t VALUES (1)');
    });
    const stored = new Map(map);
    const reader = sqlAttempt({ map, shared: true, id: 'reader' });
    expect(await reader.end(() => database(reader.db, 'library').values('SELECT x FROM t'))).toEqual([[1]]);
    expect(map).toEqual(stored);
  });

  it('is refused once the handler ended', async () => {
    const attempt = sqlAttempt({ map: new Map() });
    await attempt.end(async () => {});
    expect(() => database(attempt.db, 'library')).toThrow(/ended/);
  });
});

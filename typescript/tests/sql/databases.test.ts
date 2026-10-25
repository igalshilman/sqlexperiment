// A key's databases apart from the object's own (databaseOf of restate-fns),
// without a Restate server: each its own tables, stored in the object's state
// beside `ctx.db`'s, opened on first use and ended with the attempt.
import { databaseOf } from 'restate-fns/internal';
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from '../testing.ts';

describe("a key's other database", () => {
  it('keeps tables apart from the object’s own, through attempts', async () => {
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map });
    await first.end(async () => {
      await first.db.execute('CREATE TABLE notes (text TEXT)');
      const other = databaseOf(first.db, 'library');
      await other.execute('CREATE TABLE notes (text TEXT)');
      await other.execute("INSERT INTO notes VALUES ('its own')");
    });
    const later = sqlAttempt({ map, id: 'later' });
    const [yours, its] = await later.end(async () => [
      await later.db.query('SELECT text FROM notes'),
      await databaseOf(later.db, 'library').query('SELECT text FROM notes'),
    ]);
    expect(yours).toEqual([]);
    expect(its).toEqual([['its own']]);
  });

  it('is stored under the format’s prefix, with its name', async () => {
    const map = new Map<string, Uint8Array>();
    const attempt = sqlAttempt({ map });
    await attempt.end(() => databaseOf(attempt.db, 'library').execute('CREATE TABLE t (x)'));
    expect([...map.keys()].filter((key) => !key.startsWith('v17/library/'))).toEqual([]);
    expect(map.has('v17/library/root')).toBe(true);
  });

  it('reads in a shared handler, storing nothing', async () => {
    const map = new Map<string, Uint8Array>();
    const writer = sqlAttempt({ map });
    await writer.end(async () => {
      const other = databaseOf(writer.db, 'library');
      await other.execute('CREATE TABLE t (x)');
      await other.execute('INSERT INTO t VALUES (1)');
    });
    const stored = new Map(map);
    const reader = sqlAttempt({ map, shared: true, id: 'reader' });
    expect(await reader.end(() => databaseOf(reader.db, 'library').query('SELECT x FROM t'))).toEqual([[1]]);
    expect(map).toEqual(stored);
  });

  it('is refused once the handler ended', async () => {
    const attempt = sqlAttempt({ map: new Map() });
    await attempt.end(async () => {});
    expect(() => databaseOf(attempt.db, 'library')).toThrow(/ended/);
  });
});

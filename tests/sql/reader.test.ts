// `ctx.db` in a shared handler, without a Restate server: it reads the
// object's database beside the exclusive handler, which commits meanwhile,
// as a snapshot: the state it opened on, whatever the exclusive handler
// commits; and what it writes stays in it.
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from '../testing.ts';

/** An object's state, with a table of numbers: its exclusive handler's attempt, and its shared handlers'. */
async function object() {
  const map = new Map<string, Uint8Array>();
  const owner = sqlAttempt({ map, id: 'owner' });
  await owner.db.execute('CREATE TABLE numbers (n INTEGER)');
  await owner.db.execute('INSERT INTO numbers VALUES (1)');
  return { owner, reader: (id = 'reader') => sqlAttempt({ map, id, shared: true }) };
}

describe("a shared handler's ctx.db", () => {
  it('reads what the exclusive handler committed, while it runs', async () => {
    const { reader } = await object();
    const shared = reader();
    expect(await shared.end(() => shared.db.query('SELECT n FROM numbers'))).toEqual([[1]]);
  });

  it('keeps what it writes to itself', async () => {
    const { reader } = await object();
    const first = reader();
    await first.end(async () => {
      await first.db.execute('CREATE TABLE IF NOT EXISTS numbers (n INTEGER)');
      await first.db.execute('INSERT INTO numbers VALUES (99)');
    });
    const second = reader('again');
    expect(await second.end(() => second.db.query('SELECT n FROM numbers'))).toEqual([[1]]);
  });

  it('reads the state it opened on, whatever the exclusive handler commits meanwhile', async () => {
    const { owner, reader } = await object();
    const shared = reader();
    const reads = shared.end(async () => {
      const before = await shared.db.query('SELECT n FROM numbers');
      await owner.db.execute('INSERT INTO numbers VALUES (2)');
      return [before, await shared.db.query('SELECT n FROM numbers ORDER BY n')];
    });
    expect(await reads).toEqual([[[1]], [[1]]]);
    const later = reader('later');
    expect(await later.end(() => later.db.query('SELECT n FROM numbers ORDER BY n'))).toEqual([[1], [2]]);
  });

  it('has no transaction of its own', async () => {
    const { reader } = await object();
    const shared = reader();
    await expect(shared.end(() => shared.transaction(async () => {}))).rejects.toThrow(
      'ctx.transaction is for exclusive handlers',
    );
  });
});

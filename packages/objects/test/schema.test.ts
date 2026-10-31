// An object's schema (schema.ts of restate-objects), without a Restate server: its
// steps, applied once per key by the handlers that open its database.
import { randomUUID } from 'node:crypto';
import * as restate from 'restate-objects';
import { open, Schema } from 'restate-objects/internal';
import { describe, expect, it } from 'vitest';
import { memoryStore, sqlAttempt } from './support.ts';

/** A new object's name, its schema `steps`. */
const objectWith = (...steps: restate.Step[]) => {
  const name = `schema-${randomUUID()}`;
  restate.sessionObject({ name, schema: steps, handlers: {}, views: {} });
  return name;
};

const NOTES = 'CREATE TABLE notes (text TEXT NOT NULL)';

/** How many steps of the schema `name` the database in `map` has, as stored: its record. */
async function stepsOf(map: Map<string, Uint8Array>, name: string) {
  const db = await open(`schema-${randomUUID()}`, memoryStore(map));
  const record = await db.record('schema');
  await db.close();
  const applied = record.length === 0 ? {} : (JSON.parse(new TextDecoder().decode(record)) as Record<string, number>);
  return applied[name] === undefined ? '' : String(applied[name]);
}

const TABLES = "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name";

/** A step of data: the notes so far are done. */
const done = (tx: restate.DatabaseWriter) => tx.execute('UPDATE notes SET done = 1');

describe("an object's schema", () => {
  it('is applied once, where a key is first opened', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map, service });
    await first.end(() => first.db.execute("INSERT INTO notes VALUES ('milk')"));
    const later = sqlAttempt({ map, service, id: 'later' });
    // The database holds the object's tables, and only them: how far the
    // schema got is a record beside them.
    expect(await later.end(() => later.db.values(TABLES))).toEqual([['notes']]);
    expect(await stepsOf(map, service)).toBe('1');
  });

  it('runs a step added later on each existing database, once', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map, service });
    await first.end(() => first.db.execute("INSERT INTO notes VALUES ('milk')"));
    restate.sessionObject({
      name: service,
      schema: [NOTES, 'ALTER TABLE notes ADD COLUMN done INT', done],
      handlers: {},
      views: {},
    });
    const later = sqlAttempt({ map, service, id: 'later' });
    expect(await later.end(() => later.db.values('SELECT text, done FROM notes'))).toEqual([['milk', 1]]);
  });

  it('reads a key never written as empty tables, in a shared handler, storing nothing', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, service, shared: true });
    expect(await reader.end(() => reader.db.values('SELECT text FROM notes'))).toEqual([]);
    expect(map.size).toBe(0);
  });

  it('is applied by a writer after a shared handler read a key never written', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, service, shared: true, id: 'reader' });
    await reader.end(() => reader.db.values('SELECT text FROM notes'));
    const writer = sqlAttempt({ map, service, id: 'writer' });
    await writer.end(() => writer.db.execute("INSERT INTO notes VALUES ('milk')"));
    const later = sqlAttempt({ map, service, id: 'later' });
    expect(await later.end(() => later.db.values('SELECT text FROM notes'))).toEqual([['milk']]);
  });
});

describe("a library's schema", () => {
  const LIBRARY = new Schema('library', ['CREATE TABLE library_items (item TEXT)']);

  it('is ensured where it is used, beside the object’s own', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const attempt = sqlAttempt({ map, service });
    const tables = await attempt.end(async () => {
      await LIBRARY.ensure(attempt.db);
      await attempt.db.execute("INSERT INTO library_items VALUES ('a')");
      return attempt.db.values(TABLES);
    });
    expect(tables).toEqual([['library_items'], ['notes']]);
    expect([await stepsOf(map, 'library'), await stepsOf(map, service)]).toEqual(['1', '1']);
  });

  it('is applied once when ensured twice at once', async () => {
    const map = new Map<string, Uint8Array>();
    const attempt = sqlAttempt({ map });
    await attempt.end(() => Promise.all([LIBRARY.ensure(attempt.db), LIBRARY.ensure(attempt.db)]));
    expect(await stepsOf(map, 'library')).toBe('1');
  });

  it('is a shared handler’s own: empty, and stored nowhere', async () => {
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, shared: true });
    const rows = await reader.end(async () => {
      await LIBRARY.ensure(reader.db);
      return reader.db.values('SELECT item FROM library_items');
    });
    expect(rows).toEqual([]);
    expect(map.size).toBe(0);
  });
});

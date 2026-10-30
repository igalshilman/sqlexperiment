// An object's schema (schema.ts of restate-objects), without a Restate server: its
// steps, applied once per key by the handlers that open its database.
import { randomUUID } from 'node:crypto';
import * as restate from 'restate-objects';
import { Schema } from 'restate-objects/internal';
import { describe, expect, it } from 'vitest';
import { sqlAttempt } from './support.ts';

/** A new object's name, its schema `steps`. */
const objectWith = (...steps: restate.Step[]) => {
  const name = `schema-${randomUUID()}`;
  restate.object({ name, schema: steps, handlers: {} });
  return name;
};

const NOTES = 'CREATE TABLE notes (text TEXT NOT NULL)';

/** A step of data: the notes so far are done. */
const done = (tx: restate.SqlDatabase) => tx.execute('UPDATE notes SET done = 1');

describe("an object's schema", () => {
  it('is applied once, where a key is first opened', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map, service });
    await first.end(() => first.db.execute("INSERT INTO notes VALUES ('milk')"));
    const later = sqlAttempt({ map, service, id: 'later' });
    expect(await later.end(() => later.db.query('SELECT name, version FROM restate_schema'))).toEqual([[service, 1]]);
  });

  it('runs a step added later on each existing database, once', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const first = sqlAttempt({ map, service });
    await first.end(() => first.db.execute("INSERT INTO notes VALUES ('milk')"));
    restate.object({ name: service, schema: [NOTES, 'ALTER TABLE notes ADD COLUMN done INT', done], handlers: {} });
    const later = sqlAttempt({ map, service, id: 'later' });
    expect(await later.end(() => later.db.query('SELECT text, done FROM notes'))).toEqual([['milk', 1]]);
  });

  it('reads a key never written as empty tables, in a shared handler, storing nothing', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, service, shared: true });
    expect(await reader.end(() => reader.db.query('SELECT text FROM notes'))).toEqual([]);
    expect(map.size).toBe(0);
  });

  it('is applied by a writer after a shared handler read a key never written', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, service, shared: true, id: 'reader' });
    await reader.end(() => reader.db.query('SELECT text FROM notes'));
    const writer = sqlAttempt({ map, service, id: 'writer' });
    await writer.end(() => writer.db.execute("INSERT INTO notes VALUES ('milk')"));
    const later = sqlAttempt({ map, service, id: 'later' });
    expect(await later.end(() => later.db.query('SELECT text FROM notes'))).toEqual([['milk']]);
  });
});

describe("a library's schema", () => {
  const LIBRARY = new Schema('library', ['CREATE TABLE library_items (item TEXT)']);

  it('is ensured where it is used, beside the object’s own', async () => {
    const service = objectWith(NOTES);
    const map = new Map<string, Uint8Array>();
    const attempt = sqlAttempt({ map, service });
    const versions = await attempt.end(async () => {
      await LIBRARY.ensure(attempt.db);
      await attempt.db.execute("INSERT INTO library_items VALUES ('a')");
      return attempt.db.query('SELECT name, version FROM restate_schema ORDER BY name');
    });
    expect(versions).toEqual([
      ['library', 1],
      [service, 1],
    ]);
  });

  it('is a shared handler’s own: empty, and stored nowhere', async () => {
    const map = new Map<string, Uint8Array>();
    const reader = sqlAttempt({ map, shared: true });
    const rows = await reader.end(async () => {
      await LIBRARY.ensure(reader.db);
      return reader.db.query('SELECT item FROM library_items');
    });
    expect(rows).toEqual([]);
    expect(map.size).toBe(0);
  });
});

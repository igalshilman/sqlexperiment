// Notes in SQL: each key has a database of its own, kept in Restate with
// the object's memory. Statements are synchronous, and commit together with
// the memory: a crash keeps both, or neither.
//
//   curl localhost:8080/notes/alice/add --json '"Buy milk"'
//   curl localhost:8080/notes/alice/done --json 1
//   curl localhost:8080/notes/alice/list
export function add(text, ctx) {
  table(ctx);
  ctx.db.sql`INSERT INTO notes (text) VALUES (${text})`;
  return list(null, ctx);
}

export function done(id, ctx) {
  table(ctx);
  ctx.db.sql`UPDATE notes SET done = 1 WHERE id = ${id}`;
  return list(null, ctx);
}

export function list(_, ctx) {
  table(ctx);
  return ctx.db.sql`SELECT id, text, done FROM notes ORDER BY id`;
}

function table(ctx) {
  ctx.db.sql`CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY, text TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)`;
}

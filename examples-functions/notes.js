// Notes in SQL: each scope (the object key) has a database of its own, kept
// in Restate with the run. Statements are synchronous, and commit with the
// turn: a crash keeps the notes and the run's memory together.
//
//   curl localhost:8080/notes/alice/run --json '{"add": "Buy milk"}'
//   curl localhost:8080/notes/alice/run --json '{"done": 1}'
//   curl localhost:8080/notes/alice/run --json '{}'
async function main({ add, done }, ctx) {
  ctx.db.sql`CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY, text TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)`;
  if (add) ctx.db.sql`INSERT INTO notes (text) VALUES (${add})`;
  if (done) ctx.db.sql`UPDATE notes SET done = 1 WHERE id = ${done}`;
  return ctx.db.sql`SELECT id, text, done FROM notes ORDER BY id`;
}

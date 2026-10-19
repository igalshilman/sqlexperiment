"""Notes in SQL: each key has a database of its own, kept in Restate with the
object's memory. Statements are synchronous, and commit together with the
memory: a crash keeps both, or neither. A t-string's {values} are
parameters, never SQL text.

    curl localhost:8080/notes/alice/add --json '"Plant tomatoes"'
    curl localhost:8080/notes/alice/done --json 1
    curl localhost:8080/notes/alice/search --json '"tomato"'
    curl localhost:8080/notes/alice/show
"""


def add(text, ctx):
    _table(ctx)
    ctx.db.sql(t"INSERT INTO notes (text) VALUES ({text})")
    return show(None, ctx)


def done(id, ctx):
    _table(ctx)
    ctx.db.sql(t"UPDATE notes SET done = 1 WHERE id = {id}")
    return show(None, ctx)


def search(word, ctx):
    _table(ctx)
    pattern = f"%{word}%"
    return ctx.db.sql(t"SELECT id, text, done FROM notes WHERE text LIKE {pattern} ORDER BY id")


def show(_, ctx):
    _table(ctx)
    return ctx.db.sql(t"SELECT id, text, done FROM notes ORDER BY id")


def _table(ctx):
    ctx.db.sql(t"""CREATE TABLE IF NOT EXISTS notes (
        id INTEGER PRIMARY KEY, text TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)""")

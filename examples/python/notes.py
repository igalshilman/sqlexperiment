"""
SQL in a virtual object: every key has a SQLite database of its own,
stored in Restate with the object's state. No database to run.

    uv run examples/python/notes.py
    restate deployments register http://localhost:9080
    curl localhost:8080/notes/alice/add --json '"Buy milk"'
    curl localhost:8080/notes/alice/list
"""

import restate
from restate import ObjectContext
from serve import serve

notes = restate.VirtualObject("notes", **restate.sql_options)

SCHEMA = "CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)"


@notes.handler()
async def add(ctx: ObjectContext, note: str) -> None:
    await ctx.db.execute(SCHEMA)
    await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])


@notes.handler("list")
async def list_notes(ctx: ObjectContext) -> list[str]:
    await ctx.db.execute(SCHEMA)
    rows = await ctx.db.query("SELECT text FROM notes ORDER BY rowid")
    return [text for [text] in rows]


if __name__ == "__main__":
    serve(notes)

"""
Notes in SQL: every key of the object has a SQLite database of its own,
stored in Restate with the object's state. No database to run.

    uv run basic/notes.py
    restate deployments register http://localhost:9080
    curl localhost:8080/notes/alice/add --json '"Buy milk"'
    curl localhost:8080/notes/alice/list
"""

import restate_fns as restate
from restate_agents import serve

notes = restate.VirtualObject("notes")


@notes.handler()
async def add(ctx: restate.ObjectContext, note: str):
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)")
    await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])


@notes.handler("list")
async def list_notes(ctx: restate.ObjectContext) -> list[str]:
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)")
    return [text for (text,) in await ctx.db.query("SELECT text FROM notes ORDER BY rowid")]


if __name__ == "__main__":
    serve(notes)

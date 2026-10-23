"""
The pair's tools, beside the sandbox's own (list, read and write files, and
run a command): an edit of a file in place, and the plan, a table of the
project's database that the pair keeps up to date as it works.

The sandbox is a Modal Sandbox per project with MODAL_TOKEN_ID set, and
otherwise a directory of this machine (``sandbox.local()``), which isolates
nothing: for trying it out.
"""

import os

from restate_agents import sandbox, tool

box = sandbox.modal(image="python:3.13") if os.environ.get("MODAL_TOKEN_ID") else sandbox.local()

# The plan's table, the project's own, beside the agent's: the pair's schema.
TASKS = ["CREATE TABLE tasks (n INTEGER PRIMARY KEY, task TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)"]


@tool
async def edit_file(ctx, path: str, old: str, new: str) -> str:
    """Edit a text file of the sandbox's workspace: replace ``old``, which occurs in it once, with ``new``."""
    workspace = await box.workspace(ctx)
    text = await workspace.read_file(path)
    if (count := text.count(old)) != 1:
        return f"{path} has that text {count} times: give text that occurs once"
    await workspace.write_file(path, text.replace(old, new))
    return f"Edited {path}"


@tool
async def plan(ctx, tasks: list[str]) -> list[dict]:
    """Set the plan of the work you were asked for: its tasks, in order, in place of the plan before."""
    async with ctx.db.transaction() as tx:
        await tx.execute("DELETE FROM tasks")
        for task in tasks:
            await tx.execute("INSERT INTO tasks (task) VALUES (?)", [task])
    return await tasks_of(ctx.db)


@tool
async def done(ctx, n: int) -> list[dict]:
    """Mark the plan's task ``n`` (from 1) done."""
    await ctx.db.execute("UPDATE tasks SET done = 1 WHERE n = ?", [n])
    return await tasks_of(ctx.db)


async def tasks_of(db) -> list[dict]:
    """The plan: each task's number, text, and whether it is done."""
    rows = await db.query("SELECT n, task, done FROM tasks ORDER BY n")
    return [{"n": n, "task": task, "done": bool(finished)} for n, task, finished in rows]

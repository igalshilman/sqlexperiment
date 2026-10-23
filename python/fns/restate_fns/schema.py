"""
The schema of an object's databases: each key's database is made by the
same steps, in order, applied once per key, where its first handler opens
it. A database keeps how far it got in the table ``restate_schema``, so a
step added later (a column, an index) runs on each key's database once, as
that key is next opened::

    notes = restate.VirtualObject("notes", schema=[
        "CREATE TABLE notes (text TEXT NOT NULL)",
        "ALTER TABLE notes ADD COLUMN at TEXT",
    ])

A step is a statement, or an async function of the transaction (to move
data, say). Steps are never changed or removed once they ran: a change is a
step of its own.

A shared handler applies the steps its key's database lacks in its own
session, which stores nothing, then reads: a key never written reads as
empty tables. A library keeps its tables in a schema of its own name,
which ``schema.ensure(ctx.db)`` applies where it uses them, in any object.
"""

from __future__ import annotations

import weakref
from collections.abc import Awaitable, Callable, Sequence
from dataclasses import dataclass
from typing import Any

TABLE = "restate_schema"

Step = str | Callable[[Any], Awaitable[None]]
"""A statement, or an async function of the transaction."""


@dataclass(frozen=True)
class Schema:
    """A part of a database's schema: its ``name``, and the ``steps`` that make it, in order."""

    name: str
    steps: tuple[Step, ...]

    def __init__(self, name: str, steps: Sequence[Step]) -> None:
        object.__setattr__(self, "name", name)
        object.__setattr__(self, "steps", tuple(steps))

    async def ensure(self, db: Any) -> None:
        """
        Apply the steps ``db`` lacks: a library's tables, where it uses them,
        in any object. On a handler's ``ctx.db``, once per attempt, and a
        shared handler's are its own, vanishing with its session.
        """
        session = sessions[db]() if db in sessions else None
        await (session([self]) if session is not None else apply(db, [self]))


# The handlers' sessions (``ctx.db``), each with how its attempt applies schemas: a weak method, so
# that the attempt, which holds its session, is not kept alive by it.
sessions: weakref.WeakKeyDictionary[Any, weakref.WeakMethod] = weakref.WeakKeyDictionary()


# The schema of each object, by its name.
_objects: dict[str, Schema] = {}


def register(name: str, steps: Sequence[Step]) -> None:
    """The object ``name``'s own schema: its steps, which each of its databases is made by."""
    _objects[name] = Schema(name, steps)


def of_object(name: str) -> Schema | None:
    """The object ``name``'s own schema, if it has one."""
    return _objects.get(name)


async def apply(db: Any, schemas: Sequence[Schema]) -> None:
    """Apply the steps of ``schemas`` that ``db`` lacks, in one transaction, with how far each got."""
    applied = await _applied(db)
    behind = [s for s in schemas if applied.get(s.name, 0) < len(s.steps)]
    if not behind:
        return
    async with db.transaction() as tx:
        await tx.execute(f"CREATE TABLE IF NOT EXISTS {TABLE} (name TEXT PRIMARY KEY, version INTEGER NOT NULL)")
        for schema in behind:
            for step in schema.steps[applied.get(schema.name, 0) :]:
                await (tx.execute(step) if isinstance(step, str) else step(tx))
            await tx.execute(f"INSERT OR REPLACE INTO {TABLE} VALUES (?, ?)", [schema.name, len(schema.steps)])


async def _applied(db: Any) -> dict[str, int]:
    """How many steps of each schema ``db`` has."""
    if not await db.query("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", [TABLE]):
        return {}
    return {name: version for name, version in await db.query(f"SELECT name, version FROM {TABLE}")}

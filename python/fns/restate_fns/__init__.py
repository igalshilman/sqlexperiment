"""
The Restate Python SDK, with SQL in virtual objects::

    import restate_fns as restate

    notes = restate.VirtualObject("notes")


    @notes.handler()
    async def add(ctx: restate.ObjectContext, note: str) -> None:
        await ctx.db.execute("CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)")
        await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])

It exports what ``restate`` does, with ``VirtualObject``, ``ObjectContext``
and ``ObjectSharedContext`` of its own: see README.md.
"""

# pylint: disable=wildcard-import,unused-wildcard-import
from restate import *  # noqa: F403
from restate import __all__ as _restate_all

from restate_fns.object import (
    JournaledObjectContext,
    JournaledObjectSharedContext,
    ObjectContext,
    ObjectSharedContext,
    VirtualObject,
)
from restate_fns.schema import Schema
from restate_fns.session import (
    SqlDatabase,
    SqlOptions,
    SqlSession,
    SqlTransaction,
    SqlTransactionSends,
    sql_options,
)

__all__ = [
    *_restate_all,
    # Its own, in place of restate's:
    "ObjectContext",
    "ObjectSharedContext",
    "VirtualObject",
    "JournaledObjectContext",
    "JournaledObjectSharedContext",
    "Schema",
    "SqlDatabase",
    "SqlOptions",
    "SqlSession",
    "SqlTransaction",
    "SqlTransactionSends",
    "sql_options",
]

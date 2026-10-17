"""Virtual objects set up for SQL and agents."""

from __future__ import annotations

import asyncio
import os
from datetime import timedelta
from typing import Any

import hypercorn.asyncio
import hypercorn.config
import restate
from restate import InvocationRetryPolicy, ObjectContext

# Restate's UI hides the SQL storage from the journal view: the database's
# pages, log and header in the object's state (v13/...), the sends pending
# with a transaction, and the `sync` run before an attempt ends.
METADATA = {
    "dev.restate.tooling.journal.hidden.names": "sync,_restate_sql_sends",
    "dev.restate.tooling.journal.hidden.name.prefix": "v13/",
}

# Each attempt resumes from the database, so a failed one is retried soon, and without end.
RETRY_POLICY = InvocationRetryPolicy(
    initial_interval=timedelta(milliseconds=50), max_interval=timedelta(milliseconds=500), max_attempts=100_000
)


def object(name: str, *, description: str | None = None, metadata: dict[str, str] | None = None, **options: Any):
    """
    A Restate virtual object with a SQL database of its own, ``ctx.db``: one
    per key, stored with the object's state::

        notes = restate_agents.object("notes")

        @notes.handler()
        async def add(ctx: ObjectContext, note: str) -> None:
            await ctx.db.execute("INSERT INTO notes VALUES (?)", [note])

    ``options`` add to (or replace) the ones set here: ``restate.sql_options``
    and a retry policy without end. ``metadata`` adds to the metadata that
    hides the SQL storage in Restate's UI.
    """
    return restate.VirtualObject(
        name,
        description=description,
        metadata={**METADATA, **(metadata or {})},
        **{**restate.sql_options, "invocation_retry_policy": RETRY_POLICY, **options},
    )


def context() -> ObjectContext:
    """The context of the handler running, such as for a tool to call a service or use ``ctx.db``."""
    from restate.extensions import current_context

    ctx = current_context()
    if not isinstance(ctx, ObjectContext):
        raise RuntimeError("not in a Restate object's handler")
    return ctx


def serve(*services: restate.Service | restate.VirtualObject | restate.Workflow) -> None:
    """Serve ``services`` over HTTP/2, on PORT or 9080, until interrupted."""
    config = hypercorn.config.Config()
    config.bind = [f"0.0.0.0:{os.environ.get('PORT', '9080')}"]
    asyncio.run(hypercorn.asyncio.serve(restate.app(list(services)), config))  # type: ignore[arg-type]

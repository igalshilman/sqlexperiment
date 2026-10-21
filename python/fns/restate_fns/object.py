"""
A virtual object whose handlers have SQL: ``ctx.db`` and ``ctx.transaction()``.

Its handlers run with the SDK's explicit recovery instead of journal replay:
each attempt gets an SQL session (session.py), and the selector (selector.py)
drives its waits. A handler with ``journal_replay=True`` is the SDK's own.
"""

from __future__ import annotations

import abc
import functools
import typing
from collections.abc import Awaitable, Callable
from datetime import timedelta
from typing import Any

import restate
from restate.exceptions import SuspendedException, TerminalError
from restate.handler import handler_from_callable
from restate.serde import BytesSerde
from restate.server_context import ServerInvocationContext

from restate_fns import selector
from restate_fns.session import SqlAttempt, SqlSession, SqlTransaction, sql_options


class ObjectContext(restate.ObjectContext):
    """The context of an object's exclusive handler, with the object's database."""

    @property
    @abc.abstractmethod
    def db(self) -> SqlSession:
        """
        The object's SQL database, kept in its state: each statement is its own
        transaction, and ``db.transaction()`` groups several::

            await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])
            rows = await ctx.db.query("SELECT text FROM notes ORDER BY rowid")

        Each commit is durable once it returns. An SQL error raises a
        ``TerminalError``, since a retry would repeat it; a storage error is
        retried.
        """

    @abc.abstractmethod
    def transaction(self) -> typing.AsyncContextManager[SqlTransaction]:
        """
        One transaction of ``ctx.db``, with the sends it commits with::

            async with ctx.transaction() as t:
                await t.db.execute("UPDATE stock SET n = n - ? WHERE sku = ?", [qty, sku])
                t.tx.object_send(shipping.ship, key=sku, arg=qty)

        It commits when the body ends, and rolls back when the body raises. A
        send made with ``t.tx`` is made once the transaction commits, and only
        then: exactly once, even if the attempt crashes after the commit.
        """


class ObjectSharedContext(restate.ObjectSharedContext):
    """The context of an object's shared handler, which reads the object's database."""

    @property
    @abc.abstractmethod
    def db(self) -> SqlSession:
        """
        The object's database as stored, read beside the exclusive handler that
        writes it: a write statement raises. If a commit came while it read,
        the attempt fails, and Restate retries it.
        """


JournaledObjectContext = restate.ObjectContext
"""The context of a handler with ``journal_replay=True``: the SDK's, with no ``ctx.db``."""

JournaledObjectSharedContext = restate.ObjectSharedContext
"""The context of a shared handler with ``journal_replay=True``: the SDK's, with no ``ctx.db``."""


class VirtualObject(restate.VirtualObject):
    """
    A virtual object whose handlers have SQL, ``ctx.db``: one database per key,
    kept in the object's state::

        notes = restate.VirtualObject("notes")

        @notes.handler()
        async def add(ctx: restate.ObjectContext, note: str) -> None:
            await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])

    It takes the options of ``restate.VirtualObject``, which add to (or replace)
    ``sql_options``. A handler with ``journal_replay=True`` is the SDK's own,
    with no ``ctx.db``, such as one that waits on an awakeable.
    """

    def __init__(self, name: str, **options: Any) -> None:
        super().__init__(name, **{**sql_options, **options})

    def handler(self, *args: Any, journal_replay: bool = False, **options: Any) -> Callable[[Any], Any]:
        """
        A handler, as ``restate.VirtualObject.handler`` makes it: with
        ``ctx.db``, and the SDK's explicit recovery, unless it replays its
        journal.
        """
        register = super().handler(*args, **options)
        if journal_replay:
            return register

        def register_with_database(fn: Callable[..., Awaitable[Any]]) -> Any:
            registered = register(with_database(fn))
            handler_from_callable(registered).explicit_recovery = True
            return registered

        return register_with_database


def with_database(fn: Callable[..., Awaitable[Any]]) -> Callable[..., Awaitable[Any]]:
    """
    ``fn``, a handler without journal replay, with ``ctx.db`` and
    ``ctx.transaction()``. A context that is not the SDK's, a test's, has
    them already.
    """

    @functools.wraps(fn)
    async def handler(ctx: Any, *args: Any) -> Any:
        if not isinstance(ctx, ServerInvocationContext):
            return await fn(ctx, *args)
        attempt = SqlAttempt(_ContextHost(ctx))
        ctx.db = attempt.db  # type: ignore[attr-defined]
        ctx.transaction = attempt.transaction  # type: ignore[attr-defined]
        ctx.awakeable = _no_awakeable  # type: ignore[method-assign]
        try:
            result = await fn(ctx, *args)
        except BaseException:
            await attempt.end(failed=True)
            raise
        await attempt.end(failed=False)
        return result

    return handler


def _no_awakeable(*args: Any, **kwargs: Any) -> Any:
    # A terminal error, since a retry would fail the same way. Signal ids of
    # skipped attempts are unknown, so a new one could collide.
    raise TerminalError("awakeables are not available without journal replay: use ctx.signal")


class _ContextHost:
    """The SQL session's host (session.py's ``SqlHost``) over the SDK's context, with its selector hooked."""

    def __init__(self, context: ServerInvocationContext) -> None:
        self.context = context
        self._listeners: list[Callable[[], None]] = []
        self.selector = selector.hook(context, self._notify_attempt_end)
        attempt_finished = context.on_attempt_finished

        def on_attempt_finished() -> None:
            self._notify_attempt_end()
            attempt_finished()

        context.on_attempt_finished = on_attempt_finished  # type: ignore[method-assign]

    @property
    def sql_handler_kind(self) -> str | None:
        return self.context.handler.kind

    @property
    def sql_namespace(self) -> str:
        return f"{self.context.handler.service_tag.name}/{self.context.key()}"

    def sql_get(self, key: str) -> Awaitable[bytes | None]:
        # Each entry is named after its key, so that tooling can hide a
        # database's pages and log by name.
        # TODO: read with protocol v8's ephemeral get, left out of the journal,
        # once a Restate release ships v8 and the SDK exposes the get (FUTURE.md
        # at the repository's root). Restate 1.8 negotiates v7 at most.
        return self.context.get(key, serde=BytesSerde(), entry_name=key)

    def sql_set(self, key: str, value: bytes) -> None:
        self.context.set(key, value, serde=BytesSerde(), entry_name=key)

    def sql_clear(self, key: str) -> None:
        self.context.clear(key, entry_name=key)

    def sql_sync(self) -> Awaitable[Any]:
        return self.context.run_typed("sync", _nothing)

    def sql_send(
        self,
        service: str,
        handler: str,
        parameter: bytes,
        key: str | None,
        send_delay: timedelta | None,
        idempotency_key: str | None,
        headers: dict[str, str] | None,
    ) -> None:
        self.context.generic_send(service, handler, parameter, key, send_delay, idempotency_key, headers)

    def sql_attempt_end(self) -> BaseException | None:
        ended = self.selector.ended
        if ended is not None:
            return ended
        if self.context.request_finished_event.is_set():
            return SuspendedException()
        return None

    def sql_on_attempt_end(self, listener: Callable[[], None]) -> Callable[[], None]:
        if self.sql_attempt_end() is not None:
            listener()
            return lambda: None
        self._listeners.append(listener)

        def cancel() -> None:
            if listener in self._listeners:
                self._listeners.remove(listener)

        return cancel

    def _notify_attempt_end(self) -> None:
        listeners, self._listeners = self._listeners, []
        for listener in listeners:
            listener()


async def _nothing() -> None:
    """The action of an empty run."""

"""
An attempt's SQL session: ``ctx.db``, a database kept in the object's state,
through the sqlite-kv extension, with no journal replay; and
``ctx.transaction()``, one transaction of it with the sends it commits with.
An attempt has one session, opened on first use and saved when the handler
ends. It needs of the handler's context what ``SqlHost`` lists (object.py
gives it, over the SDK's context).
"""

from __future__ import annotations

import asyncio
import base64
import json
import typing
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Sequence
from contextlib import asynccontextmanager
from datetime import timedelta
from typing import Any, Protocol

import sqlite_kv
from restate.context import HandlerType, SendHandle
from restate.exceptions import TerminalError
from restate.handler import Handler, handler_from_callable

In = typing.TypeVar("In")
Out = typing.TypeVar("Out")

SqlValue = None | int | float | str | bytes | bool
"""An SQL value. A ``bool`` parameter is stored as 0 or 1; BLOBs come back as ``bytes``."""

Rows = list[list[Any]]
"""
Result rows: lists of ``SqlValue``s, typed ``Any`` (as ``sqlite3``'s), so
that a row unpacks to what its query selects.
"""


class SqlOptions(typing.TypedDict):
    """The options of an object whose handlers use ``ctx.db``: see ``sql_options``."""

    enable_lazy_state: bool
    journal_retention: timedelta


sql_options: SqlOptions = {
    "enable_lazy_state": True,
    "journal_retention": timedelta(0),
}
"""
The options of an object whose handlers use ``ctx.db``, which
``restate_fns.VirtualObject`` sets: lazy state, so an invocation does not
start by loading every page; and no journal kept after an invocation
completes, since it holds every page it read. Its handlers also run with
explicit recovery, since a later attempt makes different reads.
"""


class SqlDatabase(Protocol):
    """SQL: the object's database, or one transaction of it."""

    async def query(self, sql: str, params: Sequence[Any] | None = None) -> Rows:
        """Run SQL and return its rows; parameters bind to a single statement."""
        ...

    async def query_columns(self, sql: str, params: Sequence[Any] | None = None) -> tuple[list[str], Rows]:
        """``query``, returning ``(columns, rows)``: also the column names of the statement whose rows these are."""
        ...

    async def execute(self, sql: str, params: Sequence[Any] | None = None) -> None:
        """Run SQL (one or more statements), discarding rows."""
        ...


class SqlSession(SqlDatabase, Protocol):
    """
    The object's database, ``ctx.db``, for the whole handler: each statement
    outside ``transaction()`` is its own transaction. Calls run one at a time,
    in the order they start.
    """

    def transaction(self) -> typing.AsyncContextManager[SqlDatabase]:
        """
        One SQL transaction, as the body of an ``async with``::

            async with ctx.db.transaction() as db:
                await db.execute("UPDATE accounts SET balance = balance - ? WHERE id = ?", [amount, src])
                await db.execute("UPDATE accounts SET balance = balance + ? WHERE id = ?", [amount, dst])

        It commits when the body ends, and rolls back when the body raises. The
        transaction holds the session: other calls to it wait until it ends,
        and a call from inside the body is refused, as it would wait for the
        body. So the body uses the ``db`` it gets, which is invalid once the
        body has ended.
        """
        ...


class SqlTransactionSends(Protocol):
    """
    The sends of a transaction: they are made once it commits, and only then.
    They have no invocation id.
    """

    def service_send(
        self,
        tpe: HandlerType[In, Out],
        arg: In,
        send_delay: timedelta | None = None,
        idempotency_key: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> SendHandle:
        """Send a message to a service, once the transaction commits."""
        ...

    def object_send(
        self,
        tpe: HandlerType[In, Out],
        key: str,
        arg: In,
        send_delay: timedelta | None = None,
        idempotency_key: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> SendHandle:
        """Send a message to an object, once the transaction commits."""
        ...

    def workflow_send(
        self,
        tpe: HandlerType[In, Out],
        key: str,
        arg: In,
        send_delay: timedelta | None = None,
        idempotency_key: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> SendHandle:
        """Send a message to a workflow, once the transaction commits."""
        ...

    def generic_send(
        self,
        service: str,
        handler: str,
        arg: bytes,
        key: str | None = None,
        send_delay: timedelta | None = None,
        idempotency_key: str | None = None,
        headers: dict[str, str] | None = None,
    ) -> SendHandle:
        """Send a message to a service, object or workflow by name, once the transaction commits."""
        ...


class SqlTransaction(typing.NamedTuple):
    """What the body of ``async with ctx.transaction() as t`` gets."""

    db: SqlDatabase
    """The transaction's SQL."""
    tx: SqlTransactionSends
    """The transaction's sends."""


class SqlHost(Protocol):
    """What the SQL session needs of the handler's context."""

    @property
    def sql_handler_kind(self) -> str | None:
        """The handler's kind, if it is an object's: "exclusive" or "shared"."""
        ...

    @property
    def sql_namespace(self) -> str:
        """The object's database's name: ``service/key``."""
        ...

    def sql_get(self, key: str) -> Awaitable[bytes | None]:
        """A state read, issued now."""
        ...

    def sql_set(self, key: str, value: bytes) -> None:
        """A state write."""
        ...

    def sql_clear(self, key: str) -> None:
        """A state clear."""
        ...

    def sql_sync(self) -> Awaitable[Any]:
        """An empty run: done once every command before it is stored."""
        ...

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
        """A send, its parameter serialized."""
        ...

    def sql_attempt_end(self) -> BaseException | None:
        """Why the attempt ended (a suspension, say), or None while it goes on."""
        ...

    def sql_on_attempt_end(self, listener: Callable[[], None]) -> Callable[[], None]:
        """Call ``listener`` when the attempt ends; the returned function cancels it."""
        ...


class SqlStore(Protocol):
    """An ordered store, as the sqlite-kv extension takes it."""

    def get(self, key: str) -> Awaitable[bytes | None]: ...
    def set(self, key: str, value: bytes) -> None: ...
    def clear(self, key: str) -> None: ...
    def sync(self) -> Awaitable[Any]: ...


_wrap_store: Callable[[SqlStore], SqlStore] | None = None


def set_store_wrapper(wrap: Callable[[SqlStore], SqlStore] | None) -> None:
    """Wrap each object's store, such as for fault injection in tests."""
    global _wrap_store  # pylint: disable=global-statement
    _wrap_store = wrap


class ObjectStore:
    """
    The object's state as a store: ``set`` and ``clear`` are state writes,
    which return at once; ``sync`` is an empty run, done once the commands
    before it are stored. A ``get`` or ``sync`` that the attempt's end leaves
    waiting raises, so the session that made it ends instead of holding its
    lock.
    """

    def __init__(self, host: SqlHost) -> None:
        self._host = host

    def get(self, key: str) -> Awaitable[bytes | None]:
        return self._host.sql_get(key)

    def set(self, key: str, value: bytes) -> None:
        self._host.sql_set(key, value)

    def clear(self, key: str) -> None:
        self._host.sql_clear(key)

    def sync(self) -> Awaitable[Any]:
        return self._host.sql_sync()


class ReaderStore:
    """
    The object's state as a shared handler's store: it reads, beside the
    exclusive handler that writes, and writes nothing.
    """

    def __init__(self, host: SqlHost) -> None:
        self._host = host

    def get(self, key: str) -> Awaitable[bytes | None]:
        return self._host.sql_get(key)

    def set(self, key: str, value: bytes) -> None:
        raise TerminalError("a shared handler writes no state: ctx.db is read-only here")

    def clear(self, key: str) -> None:
        raise TerminalError("a shared handler writes no state: ctx.db is read-only here")

    async def sync(self) -> None:
        pass


# A transaction's sends commit with it, in the database's commit record (its
# effects): the outbox, which holds each committed transaction's sends until
# they are stored. They are made once the transaction commits; a later open
# makes again those still in the outbox, with the same idempotency keys
# (`transaction/<id>/<index>`, with a random id of the transaction), so making
# them again is dropped. Restate keeps an attempt's commands in the order they
# were made, so a send made before a commit is stored whenever the commit is:
# the next commit leaves the sends this attempt made out of the outbox, and so
# does the end of the attempt.


def _encode_outbox(outbox: list[dict[str, Any]]) -> bytes:
    """The outbox as effects: none is empty, as the record's effects start."""
    return json.dumps(outbox).encode() if outbox else b""


def _decode_outbox(effects: bytes) -> list[dict[str, Any]]:
    return json.loads(bytes(effects).decode()) if effects else []


class _NoInvocationId(SendHandle):
    """What a send of a transaction returns: there is no invocation yet."""

    async def invocation_id(self) -> str:
        raise TerminalError("a send of ctx.transaction has no invocation id: it is made once the transaction commits")

    async def cancel_invocation(self) -> None:
        await self.invocation_id()


class _Sends:
    """The sends of a transaction, recorded until it commits."""

    def __init__(self) -> None:
        self.recorded: list[dict[str, Any]] = []
        self.open = True

    def _record(
        self,
        service: str,
        handler: str,
        parameter: bytes,
        key: str | None,
        send_delay: timedelta | None,
        idempotency_key: str | None,
        headers: dict[str, str] | None,
    ) -> SendHandle:
        if not self.open:
            raise TerminalError("the transaction has ended: send from inside its body")
        self.recorded.append(
            {
                "service": service,
                "handler": handler,
                "key": key,
                "delay": None if send_delay is None else int(send_delay.total_seconds() * 1000),
                "idempotency_key": idempotency_key,
                "headers": headers,
                "parameter": base64.b64encode(bytes(parameter)).decode("ascii"),
            }
        )
        return _NoInvocationId()

    def _typed(self, tpe, key, arg, send_delay, idempotency_key, headers) -> SendHandle:
        target: Handler[Any, Any] = handler_from_callable(tpe)
        parameter = target.handler_io.input_serde.serialize(arg)
        return self._record(target.service_tag.name, target.name, parameter, key, send_delay, idempotency_key, headers)

    def service_send(self, tpe, arg, send_delay=None, idempotency_key=None, headers=None) -> SendHandle:
        return self._typed(tpe, None, arg, send_delay, idempotency_key, headers)

    def object_send(self, tpe, key, arg, send_delay=None, idempotency_key=None, headers=None) -> SendHandle:
        return self._typed(tpe, key, arg, send_delay, idempotency_key, headers)

    def workflow_send(self, tpe, key, arg, send_delay=None, idempotency_key=None, headers=None) -> SendHandle:
        return self._typed(tpe, key, arg, send_delay, idempotency_key, headers)

    def generic_send(
        self, service, handler, arg, key=None, send_delay=None, idempotency_key=None, headers=None
    ) -> SendHandle:
        return self._record(service, handler, arg, key, send_delay, idempotency_key, headers)


def _reraise(error: BaseException, classify: Callable[[BaseException], BaseException]) -> typing.NoReturn:
    """Raise what ``error`` fails with, as ``classify`` says."""
    classified = classify(error)
    if classified is error:
        raise error
    raise classified from error


class _View:
    """``db``'s calls, their errors classified by ``classify``."""

    def __init__(self, db: Any, classify: Callable[[BaseException], BaseException]) -> None:
        self._db = db
        self._classify = classify

    async def query(self, sql: str, params: Sequence[Any] | None = None) -> Rows:
        try:
            return await self._db.query(sql, params)
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self._classify)

    async def query_columns(self, sql: str, params: Sequence[Any] | None = None) -> tuple[list[str], Rows]:
        try:
            return await self._db.query_columns(sql, params)
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self._classify)

    async def execute(self, sql: str, params: Sequence[Any] | None = None) -> None:
        await self.query(sql, params)


class _Session:
    """``ctx.db``: the attempt's session, opened on first use."""

    def __init__(self, attempt: SqlAttempt) -> None:
        self._attempt = attempt

    async def _view(self) -> _View:
        return _View(await self._attempt.open(), self._attempt.classify)

    async def query(self, sql: str, params: Sequence[Any] | None = None) -> Rows:
        return await (await self._view()).query(sql, params)

    async def query_columns(self, sql: str, params: Sequence[Any] | None = None) -> tuple[list[str], Rows]:
        return await (await self._view()).query_columns(sql, params)

    async def execute(self, sql: str, params: Sequence[Any] | None = None) -> None:
        await self.query(sql, params)

    def transaction(self) -> typing.AsyncContextManager[SqlDatabase]:
        return self._transaction()

    @asynccontextmanager
    async def _transaction(self) -> AsyncIterator[SqlDatabase]:
        db = await self._attempt.open()
        async with self._attempt.transaction_of(db) as (tx, _):
            yield tx


class SqlAttempt:
    """
    An attempt's SQL session: opened on its first use (``ctx.db`` or
    ``ctx.transaction``), shared by everything the handler does, and saved
    when the handler ends.
    """

    def __init__(self, host: SqlHost) -> None:
        self._host = host
        self._session: asyncio.Future | None = None
        # The session, once open.
        self._db: Any = None
        self._ended = False
        self._cancel: Callable[[], None] = lambda: None
        # The ids of the outbox's transactions this attempt made the sends of.
        self._made: set[str] = set()
        self.db: SqlSession = _Session(self)
        """``ctx.db``."""

    def classify(self, error: BaseException) -> BaseException:
        """
        What a database call's ``error`` fails with: an SQL error, which a
        retry would repeat, becomes terminal, unless the session has ended (a
        storage error, which is retried). If the attempt has ended, why it did
        (a suspension, say).
        """
        ended = self._host.sql_attempt_end()
        if ended is not None:
            return ended
        if isinstance(error, TerminalError) or not isinstance(error, Exception):
            return error
        if self._db is not None and self._db.is_usable():
            terminal = TerminalError(str(error))
            terminal.__cause__ = error
            return terminal
        return error

    @asynccontextmanager
    async def transaction_of(self, db: Any) -> AsyncIterator[tuple[SqlDatabase, Any]]:
        """
        One transaction of ``db``, yielding its SQL and the extension's handle:
        the body's own error passes as it is, the transaction's (a failed
        COMMIT) is classified.
        """
        own: list[BaseException] = []
        try:
            async with db.transaction() as tx:
                try:
                    yield _View(tx, self.classify), tx
                except BaseException as error:
                    own.append(error)
                    raise
        except BaseException as error:
            if own and own[0] is error:
                raise
            _reraise(error, self.classify)

    @asynccontextmanager
    async def transaction(self) -> AsyncIterator[SqlTransaction]:
        """
        ``ctx.transaction()``: one transaction of the session, whose sends are
        made once it commits, and kept in the outbox until they are stored.
        """
        db = await self.open()
        sends = _Sends()
        outgoing: dict[str, Any] | None = None
        try:
            async with self.transaction_of(db) as (sql, tx):
                yield SqlTransaction(db=sql, tx=sends)
                sends.open = False
                # The outbox as committed, but the sends stored since, and this
                # transaction's: unchanged, it is not written again.
                outbox = await self._unstored(db)
                if sends.recorded:
                    outgoing = {"id": uuid.uuid4().hex, "sends": sends.recorded}
                    outbox.append(outgoing)
                tx.commit_effects(_encode_outbox(outbox))
        finally:
            sends.open = False
        if outgoing is not None:
            self._make(outgoing)

    def _make(self, outgoing: dict[str, Any]) -> None:
        """
        Make the sends of a committed transaction: each with an idempotency key
        of the transaction (or its own), so that making them again is dropped.
        """
        for index, send in enumerate(outgoing["sends"]):
            delay = send.get("delay")
            self._host.sql_send(
                send["service"],
                send["handler"],
                base64.b64decode(send["parameter"]),
                send.get("key"),
                None if delay is None else timedelta(milliseconds=delay),
                send.get("idempotency_key") or f"transaction/{outgoing['id']}/{index}",
                send.get("headers"),
            )
        self._made.add(outgoing["id"])

    async def _unstored(self, db: Any) -> list[dict[str, Any]]:
        """
        The outbox as committed, but the transactions this attempt made the
        sends of: a commit made after them stores them first.
        """
        return [outgoing for outgoing in _decode_outbox(await db.effects()) if outgoing["id"] not in self._made]

    async def _flush(self, db: Any) -> None:
        """
        Empty the outbox, so that a later open does not make its sends again:
        a commit after them, which stores them first.
        """
        if not await db.effects():
            return
        outbox = _encode_outbox(await self._unstored(db))
        async with db.transaction() as tx:
            tx.commit_effects(outbox)

    async def open(self) -> Any:
        """The session, opened on the first call."""
        if self._ended:
            raise RuntimeError("the handler has ended: its SQL session is closed")
        if self._session is None:
            self._session = asyncio.ensure_future(self._start())
            # Its error goes to the calls that wait for it.
            self._session.add_done_callback(lambda f: f.cancelled() or f.exception())
        return await asyncio.shield(self._session)

    def _reader(self) -> bool:
        """Whether this is a shared handler's session: it reads beside the exclusive one, and stores nothing."""
        return self._host.sql_handler_kind == "shared"

    async def _start(self) -> Any:
        host = self._host
        if host.sql_handler_kind not in ("exclusive", "shared"):
            raise TerminalError("ctx.db is for the handlers of objects")
        store: SqlStore = ReaderStore(host) if self._reader() else ObjectStore(host)
        if _wrap_store is not None:
            store = _wrap_store(store)
        try:
            db = await sqlite_kv.open(host.sql_namespace, store, reader=self._reader())
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self.classify)
        self._db = db
        # A handler still waiting for something else when the attempt ends
        # (a model request, say) keeps the session no longer: it closes once
        # no statement runs, and the handler's later calls fail.
        self._cancel = host.sql_on_attempt_end(lambda: _close_soon(db))
        if self._reader():
            # Its writes would stay its own, and vanish: a write statement fails instead.
            try:
                await db.execute("PRAGMA query_only = 1")
            except BaseException:
                await db.close()
                raise
            return db
        try:
            # What a crash may have left unmade.
            for outgoing in _decode_outbox(await db.effects()):
                self._make(outgoing)
        except BaseException:
            await db.close()
            raise
        return db

    async def end(self, failed: bool) -> None:
        """
        The handler ended (``failed``, or not): the outbox is emptied, and the
        session is saved. One that a storage error
        ended closes now.
        """
        self._ended = True
        if self._session is None:
            return
        try:
            db = await asyncio.shield(self._session)
        except BaseException:  # pylint: disable=broad-exception-caught
            # The open failed, and its error went to the call that made it.
            return
        self._cancel()
        if self._reader():
            # What it read is the stored state only if no commit came since.
            try:
                current = await db.is_current()
            except Exception:  # pylint: disable=broad-exception-caught
                current = False
            await db.close()
            if not current and not failed:
                raise RuntimeError("the object's database changed while this shared handler read it: retrying")
            return
        if failed:
            # The handler's error stands: what it committed is stored, and an
            # outbox left full is made again by the next open.
            try:
                if db.is_usable():
                    await self._flush(db)
                    await db.save()
                else:
                    await db.close()
            except BaseException:  # pylint: disable=broad-exception-caught
                try:
                    await db.close()
                except BaseException:  # pylint: disable=broad-exception-caught
                    pass
            return
        try:
            if db.is_usable():
                await self._flush(db)
                await db.save()
            else:
                await db.close()
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self.classify)


def _close_soon(db: Any) -> None:
    task = asyncio.ensure_future(db.close())
    task.add_done_callback(lambda f: f.cancelled() or f.exception())

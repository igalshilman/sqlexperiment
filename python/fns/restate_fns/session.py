"""
An attempt's SQL session: ``ctx.db``, a database kept in the object's state,
through the SQL extension, with no journal replay; ``ctx.transaction()``, one
transaction of it with the sends it commits with; and ``ctx.step(name,
body)``, one whose result the invocation keeps across its attempts. An
attempt has one session, opened on first use and saved when the handler
ends. It needs of the handler's context what ``SqlHost`` lists (object.py
gives it, over the SDK's context).
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import contextvars
import inspect
import json
import typing
import uuid
import weakref
from collections.abc import AsyncIterator, Awaitable, Callable, Sequence
from contextlib import asynccontextmanager
from datetime import timedelta
from typing import Any, Protocol

import restate_sql
from restate.context import HandlerType, SendHandle
from restate.exceptions import TerminalError
from restate.handler import Handler, handler_from_callable

from restate_fns import schema as schemas
from restate_fns.schema import Schema

In = typing.TypeVar("In")
Out = typing.TypeVar("Out")

Rows = list[list[Any]]
"""
Result rows: lists of ``restate_sql.SqlValue``s, typed ``Any`` (as
``sqlite3``'s), so that a row unpacks to what its query selects.
"""


class SqlOptions(typing.TypedDict):
    """The options of an object whose handlers use ``ctx.db``: see ``sql_options``."""

    enable_lazy_state: bool
    journal_retention: timedelta


# TODO: preload the head eagerly (`eagerStateKeysWhitelist`), see FUTURE.md.
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

    @property
    def sql_invocation_id(self) -> str:
        """The invocation's id (the request's): the same on every attempt."""
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

    def sql_keys(self) -> Awaitable[list[str]]:
        """The object's state keys, issued now: a sweep's."""
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


_wrap_store: Callable[[restate_sql.Store], restate_sql.Store] | None = None


def set_store_wrapper(wrap: Callable[[restate_sql.Store], restate_sql.Store] | None) -> None:
    """Wrap each object's store, such as for fault injection in tests."""
    global _wrap_store  # pylint: disable=global-statement
    _wrap_store = wrap


class _ObjectStore:
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

    def keys(self) -> Awaitable[list[str]]:
        return self._host.sql_keys()


class _ReaderStore:
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


# A transaction's sends commit with it, in the database's root (its
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


# The steps of the invocation in flight: each step's result, by the
# invocation's id and the step's name, committed with the step's transaction.
# The object runs one exclusive invocation at a time, so once one ends, every
# row belongs to an invocation that ended: the end of an invocation that ran
# steps, or of the next one that uses the database after a killed invocation
# left rows, drops the table. It exists only while an invocation that ran
# steps is in flight.

_STEPS = "restate_steps"


def _encode_step(value: Any) -> str | None:
    """A step's result as stored: JSON, or NULL for None."""
    return None if value is None else json.dumps(value)


def _decode_step(value: Any) -> Any:
    return None if value is None else json.loads(value)


_in_body: contextvars.ContextVar[SqlAttempt | None] = contextvars.ContextVar("restate_fns_step", default=None)
"""
The attempt whose step body runs: a call to its session from inside the body
would wait for the step's transaction, which waits for the body.
"""

_NESTED = "inside a transaction, use the transaction handle its body gets"


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
        return _View(await self._attempt.use(), self._attempt.classify)

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
        db = await self._attempt.use()
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
        # The schemas this attempt's database has all the steps of.
        self._ensured: set[str] = set()
        # The names of the invocation's steps this attempt runs or ran: a name is one step.
        self._steps: set[str] = set()
        # Whether the table of steps exists, once known.
        self._steps_table: bool | None = None
        self.db: SqlSession = _Session(self)
        """``ctx.db``."""
        schemas.sessions[self.db] = weakref.WeakMethod(self.ensure)

    async def ensure(self, schema: Sequence[Schema]) -> None:
        """Apply the steps of ``schema`` that the database lacks, each schema once per attempt."""
        lacking = [s for s in schema if s.name not in self._ensured]
        if not lacking:
            return
        db = await self.open()
        if self._reader():
            # A shared handler's writes stay its own: they make the tables it reads, then it reads only.
            await db.execute("PRAGMA query_only = 0")
        try:
            await self._apply(db, lacking)
        finally:
            if self._reader():
                await db.execute("PRAGMA query_only = 1")

    async def _apply(self, db: Any, schema: Sequence[Schema]) -> None:
        try:
            await schemas.apply(db, schema)
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self.classify)
        self._ensured.update(s.name for s in schema)

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
        db = await self.use()
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

    async def step(self, name: str, body: Callable[..., Awaitable[Out]]) -> Out:
        """
        ``ctx.step(name, body)``: ``body`` as a transaction whose result
        commits with it, as the invocation's step ``name``. A later attempt of
        the invocation gets the result, without running ``body`` again.
        """
        if self._reader():
            raise TerminalError("ctx.step is for exclusive handlers")
        if name in self._steps:
            raise TerminalError(f"ctx.step: the invocation has a step {name!r} already")
        self._steps.add(name)
        try:
            db = await self.use()
            invocation = self._host.sql_invocation_id
            if await self._has_steps(db):
                done = await _View(db, self.classify).query(
                    f"SELECT value FROM {_STEPS} WHERE invocation = ? AND name = ?", [invocation, name]
                )
                if done:
                    return _decode_step(done[0][0])
            return await self._commit_step(db, invocation, name, body)
        except BaseException:
            # Not committed: the name is free for another try.
            self._steps.discard(name)
            raise

    async def _commit_step(self, db: Any, invocation: str, name: str, body: Callable[..., Awaitable[Out]]) -> Out:
        """
        ``body``, its result and its sends committed in one transaction, which
        begins at the body's first statement (or once it returns, if it runs
        none): until then the session is free, and other calls go first.
        """
        loop = asyncio.get_running_loop()
        sends = _Sends()
        outcome: asyncio.Future[Any] = loop.create_future()
        begun: asyncio.Future[SqlDatabase] = loop.create_future()
        committed: asyncio.Task[tuple[Any, dict[str, Any] | None]] | None = None

        async def transaction() -> tuple[Any, dict[str, Any] | None]:
            async with self.transaction_of(db) as (sql, tx):
                begun.set_result(sql)
                value = await outcome
                if not self._steps_table:
                    await sql.execute(
                        f"CREATE TABLE IF NOT EXISTS {_STEPS} (invocation TEXT NOT NULL, name TEXT NOT NULL,"
                        " value TEXT, PRIMARY KEY (invocation, name))"
                    )
                await sql.execute(f"INSERT INTO {_STEPS} VALUES (?, ?, ?)", [invocation, name, _encode_step(value)])
                # The outbox as committed, but the sends stored since, and this step's.
                outbox = await self._unstored(db)
                outgoing = {"id": uuid.uuid4().hex, "sends": sends.recorded} if sends.recorded else None
                if outgoing is not None:
                    outbox.append(outgoing)
                tx.commit_effects(_encode_outbox(outbox))
            return value, outgoing

        def failed(task: asyncio.Task[Any]) -> None:
            if not begun.done():
                begun.set_exception(task.exception() or asyncio.CancelledError())

        async def begin() -> SqlDatabase:
            nonlocal committed
            if committed is None:
                committed = asyncio.ensure_future(transaction(), loop=loop)
                committed.add_done_callback(failed)
            return await asyncio.shield(begun)

        async def statement(method: str, sql: str, params: Sequence[Any] | None) -> Any:
            return await getattr(await begin(), method)(sql, params)

        class _Lazy:
            async def query(self, sql: str, params: Sequence[Any] | None = None) -> Rows:
                return await statement("query", sql, params)

            async def query_columns(self, sql: str, params: Sequence[Any] | None = None) -> tuple[list[str], Rows]:
                return await statement("query_columns", sql, params)

            async def execute(self, sql: str, params: Sequence[Any] | None = None) -> None:
                await statement("execute", sql, params)

        token = _in_body.set(self)
        try:
            t = SqlTransaction(db=_Lazy(), tx=sends)
            value = await (body(t) if inspect.signature(body).parameters else body())
            outcome.set_result(value)
        except BaseException as error:
            sends.open = False
            if committed is None:
                # A body that ran no SQL has no transaction to roll back.
                raise
            outcome.set_exception(error)
        finally:
            _in_body.reset(token)
            sends.open = False
        await begin()
        assert committed is not None
        value, outgoing = await committed
        self._steps_table = True
        if outgoing is not None:
            self._make(outgoing)
        return value

    async def _has_steps(self, db: Any) -> bool:
        """Whether the table of steps exists."""
        if self._steps_table is None:
            rows = await _View(db, self.classify).query(
                "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?", [_STEPS]
            )
            self._steps_table = bool(rows)
        return self._steps_table

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

    async def _flush(self, db: Any, keep_steps: bool) -> None:
        """
        Empty the outbox, so that a later open does not make its sends again:
        a commit after them, which stores them first. Unless ``keep_steps``,
        the same commit drops the table of steps: the invocation ends.
        """
        sending = bool(await db.effects())
        dropping = not keep_steps and await self._has_steps(db)
        if not sending and not dropping:
            return
        outbox = _encode_outbox(await self._unstored(db))
        async with db.transaction() as tx:
            if dropping:
                await tx.execute(f"DROP TABLE {_STEPS}")
            tx.commit_effects(outbox)
        self._steps_table = False

    async def use(self) -> Any:
        """The session, for a call that is not from inside a step's body."""
        if _in_body.get() is self:
            raise TerminalError(_NESTED)
        return await self.open()

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
        store: restate_sql.Store = _ReaderStore(host) if self._reader() else _ObjectStore(host)
        if _wrap_store is not None:
            store = _wrap_store(store)
        try:
            db = await restate_sql.open(host.sql_namespace, store, reader=self._reader())
        except Exception as error:  # pylint: disable=broad-exception-caught
            _reraise(error, self.classify)
        self._db = db
        # A handler still waiting for something else when the attempt ends
        # (a model request, say) keeps the session no longer: it closes once
        # no statement runs, and the handler's later calls fail.
        self._cancel = host.sql_on_attempt_end(lambda: _close_soon(db))
        own = schemas.of_object(host.sql_namespace.split("/", 1)[0])
        if self._reader():
            # Its writes would stay its own, and vanish: a write statement fails instead, once the
            # object's schema is applied (its own too: a key never written reads as empty tables).
            try:
                if own is not None:
                    await self._apply(db, [own])
                await db.execute("PRAGMA query_only = 1")
            except BaseException:
                await db.close()
                raise
            return db
        try:
            # What a crash may have left unmade.
            for outgoing in _decode_outbox(await db.effects()):
                self._make(outgoing)
            if own is not None:
                await self._apply(db, [own])
        except BaseException:
            await db.close()
            raise
        return db

    async def end(self, error: BaseException | None) -> None:
        """
        The handler ended, with ``error`` or not: the outbox is emptied, and
        the session is saved. One that a storage error ended closes now.
        Restate retries an error that is not terminal: the invocation goes on,
        and keeps its steps.
        """
        failed = error is not None
        keep_steps = failed and not isinstance(error, TerminalError)
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
            # It read the snapshot it opened on, whatever committed since; one
            # that outlived 16 commits failed its read ("is gone"), and is retried.
            await db.close()
            return
        try:
            if db.is_usable():
                await self._flush(db, keep_steps)
                await db.save()
            else:
                await db.close()
        except BaseException as saving:  # pylint: disable=broad-exception-caught
            if not failed:
                if isinstance(saving, Exception):
                    _reraise(saving, self.classify)
                raise
            # The handler's error stands: what it committed is stored, and an
            # outbox left full is made again by the next open.
            with contextlib.suppress(BaseException):
                await db.close()


def _close_soon(db: Any) -> None:
    task = asyncio.ensure_future(db.close())
    task.add_done_callback(lambda f: f.cancelled() or f.exception())

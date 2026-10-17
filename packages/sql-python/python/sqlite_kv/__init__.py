"""SQL over a key/value store implemented in Python.

A session of a namespace's database, over a store with ordered writes::

    db = await sqlite_kv.open("Ledger/a", store)
    async with db.transaction() as tx:
        await tx.execute("INSERT INTO transfers VALUES(?, ?, ?, ?)", [id, src, dst, amount])
    await db.save()  # confirms the session's writes for the page cache

See ``Store`` for the store, and README.md.
"""

from __future__ import annotations

import asyncio
import contextvars
import inspect
from collections.abc import AsyncIterator, Awaitable, Sequence
from contextlib import asynccontextmanager
from typing import Protocol

from sqlite_kv import _native

__all__ = ["Database", "Rows", "SqlParam", "SqlValue", "Store", "Transaction", "open"]

SqlValue = None | int | float | str | bytes | bool
"""An SQL value. A ``bool`` parameter is stored as 0 or 1; BLOBs come back as ``bytes``."""

SqlParam = SqlValue | bytearray | memoryview
"""An SQL parameter: an ``SqlValue``, or a BLOB as any bytes-like object."""

Rows = list[list[SqlValue]]


class Store(Protocol):
    """A key/value store with ordered writes.

    ``set`` and ``clear`` return at once, and the store keeps writes in the
    order they were issued; ``sync`` completes once every write so far is
    stored. ``get`` issues its read when called, and returns the stored value
    or this request's own latest write (``None`` when absent), or an awaitable
    of it: so a plain ``async def get`` does not do, as its read would be
    issued only when it first runs.
    """

    def get(self, key: str) -> bytes | None | Awaitable[bytes | None]: ...
    def set(self, key: str, value: bytes) -> None: ...
    def clear(self, key: str) -> None: ...
    def sync(self) -> None | Awaitable[None]: ...


async def open(namespace: str, store: Store) -> Database:  # pylint: disable=redefined-builtin
    """Open the database of ``namespace`` over ``store``.

    One process-wide page cache serves every namespace. Each SQL commit is
    durable once the store has stored its writes: a statement completes once
    they reached the store, in order, and ``save()`` waits until they are stored.
    """
    loop = asyncio.get_running_loop()
    # A failed write cannot fail its caller, which has moved on: the next sync
    # reports it, so the session's writes are not confirmed.
    failure: list[BaseException] = []

    def answer(reply, awaitable) -> None:
        def done(future: asyncio.Future) -> None:
            if future.cancelled():
                reply(None, "the store request was cancelled")
            elif future.exception() is not None:
                reply(None, str(future.exception()))
            else:
                reply(future.result())

        asyncio.ensure_future(awaitable).add_done_callback(done)

    def dispatch(op: int, key: str, value: bytes | None, reply) -> None:
        try:
            if op == _native.SET or op == _native.CLEAR:
                try:
                    if op == _native.SET:
                        store.set(key, value)  # type: ignore[arg-type]
                    else:
                        store.clear(key)
                except Exception as error:  # pylint: disable=broad-exception-caught
                    failure.append(error)
                return
            if op == _native.GET:
                result = store.get(key)
            elif failure:
                reply(None, str(failure[0]))
                return
            else:
                result = store.sync()
            if inspect.isawaitable(result):
                answer(reply, result)
            else:
                reply(None if op == _native.SYNC else result)
        except Exception as error:  # pylint: disable=broad-exception-caught
            if reply is not None:
                reply(None, str(error))

    return Database(await _native.open(namespace, dispatch, loop))


# The transaction whose body is running, in that body's context (and the
# tasks it starts): the innermost, which knows the ones it runs inside.
_running: contextvars.ContextVar[Transaction | None] = contextvars.ContextVar("sqlite_kv_running", default=None)


class Database:
    """A session of a namespace's database.

    Calls run one at a time, in the order they start, and a transaction holds
    the session until it commits or rolls back: calls made meanwhile wait, so
    none runs inside it. A call to the session from inside the transaction's
    body is refused, as it would wait for that body: the body uses its ``tx``.
    """

    def __init__(self, native) -> None:
        self._native = native
        self._lock = asyncio.Lock()

    @asynccontextmanager
    async def _serial(self) -> AsyncIterator[None]:
        running = _running.get()
        if running is not None and running.holds(self):
            raise RuntimeError("inside a transaction, use the transaction handle its body gets")
        async with self._lock:
            yield

    async def query(self, sql: str, params: Sequence[SqlParam] | None = None) -> Rows:
        """Run SQL and return its rows; parameters bind to a single statement."""
        async with self._serial():
            return await self._native.query(sql, params)

    async def query_columns(self, sql: str, params: Sequence[SqlParam] | None = None) -> tuple[list[str], Rows]:
        """Run SQL and return ``(columns, rows)``: its rows, with the column names of their statement."""
        async with self._serial():
            return await self._native.query_columns(sql, params)

    async def execute(self, sql: str, params: Sequence[SqlParam] | None = None) -> None:
        """Run SQL (one or more statements), discarding rows."""
        await self.query(sql, params)

    @asynccontextmanager
    async def transaction(self) -> AsyncIterator[Transaction]:
        """One SQL transaction, as the body of an ``async with``::

            async with db.transaction() as tx:
                await tx.execute("UPDATE stock SET n = n - 1 WHERE id = ?", [7])

        It commits when the body ends, and rolls back when the body raises; a
        COMMIT that fails rolls back too. The body uses ``tx``, which is
        invalid once the body has ended.
        """
        async with self._serial():
            native = self._native
            await native.query("BEGIN")
            tx = Transaction(self, native, _running.get())
            token = _running.set(tx)
            try:
                yield tx
            except BaseException:
                tx._end()  # pylint: disable=protected-access
                await _rollback(native)
                raise
            finally:
                _running.reset(token)
            record = tx._recorded  # pylint: disable=protected-access
            tx._end()  # pylint: disable=protected-access
            try:
                await native.commit(record)
            except BaseException:
                # A COMMIT that fails (a deferred constraint) leaves the
                # transaction open: without the rollback, the calls after it
                # would run inside it.
                await _rollback(native)
                raise

    async def version(self) -> int:
        """The database's version.

        It grows with each commit that changes the database, or whose
        transaction called ``record_commit()``, and only then. It does not
        wait for queued calls, so a transaction's body may read it: before the
        COMMIT, it is the version that the commit would advance. It waits for a
        statement that is running, and fails once the session has ended.
        """
        return await self._native.version()

    def is_usable(self) -> bool:
        """Whether SQL can still run: an SQL error leaves the session usable, a storage error ends it."""
        return self._native.is_usable()

    async def save(self) -> None:
        """End the session, after the calls before it, confirming its writes so that the page cache may trust them."""
        async with self._serial():
            await self._native.save()

    async def close(self) -> None:
        """End the session without confirming: the next one in this process starts cold.

        It does not wait for queued calls, which then fail.
        """
        await self._native.close()


async def _rollback(native) -> None:
    """Roll back the open transaction, unless a storage error ended the session.

    The caller re-raises its own error, which says why: a failing rollback
    only leaves the session unusable.
    """
    if native.is_usable():
        try:
            await native.query("ROLLBACK")
        except Exception:  # pylint: disable=broad-exception-caught
            pass


class Transaction:
    """SQL in one transaction, until it ends."""

    def __init__(self, database: Database, native, outer: Transaction | None) -> None:
        self._database = database
        self._native = native
        # The transaction whose body this one's runs in, if any: of another
        # session, since a session's own are refused.
        self._outer = outer
        self._recorded = False

    def holds(self, database: Database) -> bool:
        """Whether this transaction, or one it runs inside, is open and holds ``database``."""
        if self._native is not None and self._database is database:
            return True
        return self._outer is not None and self._outer.holds(database)

    def _check(self):
        if self._native is None:
            raise RuntimeError("the transaction has ended")
        return self._native

    def _end(self) -> None:
        self._native = None

    def record_commit(self) -> None:
        """Log this transaction's commit even if it changes nothing.

        For a body that only reads, or whose writes match no rows: the version
        then grows when it commits.
        """
        self._check()
        self._recorded = True

    async def query(self, sql: str, params: Sequence[SqlParam] | None = None) -> Rows:
        """Run SQL and return its rows; parameters bind to a single statement."""
        return await self._check().query(sql, params)

    async def query_columns(self, sql: str, params: Sequence[SqlParam] | None = None) -> tuple[list[str], Rows]:
        """Run SQL and return ``(columns, rows)``."""
        return await self._check().query_columns(sql, params)

    async def execute(self, sql: str, params: Sequence[SqlParam] | None = None) -> None:
        """Run SQL, discarding rows."""
        await self.query(sql, params)

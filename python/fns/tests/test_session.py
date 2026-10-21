"""ctx.db and ctx.transaction, over the SQL extension and a fake object state."""

import asyncio
import itertools
import json
from collections.abc import Callable
from datetime import timedelta
from typing import Any

import restate_sql
import pytest
from restate import ObjectContext, VirtualObject
from restate.exceptions import TerminalError
from restate_fns.session import SqlAttempt


@pytest.fixture(scope="session")
def anyio_backend():
    return "asyncio"


pytestmark = [pytest.mark.anyio]

_objects = itertools.count()

shop = VirtualObject("shop")


@shop.handler()
async def ship(ctx: ObjectContext, item: str) -> None:
    pass


class FakeHost:
    """
    An attempt over an object's state: reads see the stored state and this
    attempt's writes; writes, sends and syncs are recorded in order, and
    ``store`` stores the ones a crash keeps.
    """

    def __init__(self, state: dict[str, bytes], name: str, kind: str | None = "exclusive") -> None:
        self.state = state
        self.name = name
        self.kind = kind
        # ("set", key, value), ("clear", key, None), ("sync", "", None) or
        # ("send", idempotency key, (target, parameter, delay)).
        self.log: list[tuple[str, str, Any]] = []
        self.fail_reads = False

    @property
    def sql_handler_kind(self) -> str | None:
        return self.kind

    @property
    def sql_namespace(self) -> str:
        return self.name

    def sql_get(self, key: str):
        future = asyncio.get_running_loop().create_future()
        if self.fail_reads:
            future.set_exception(ConnectionError("the runtime is gone"))
            return future
        value = self.state.get(key)
        for op, k, v in self.log:
            if op in ("set", "clear") and k == key:
                value = v
        future.set_result(value)
        return future

    def sql_set(self, key: str, value: bytes) -> None:
        self.log.append(("set", key, bytes(value)))

    def sql_clear(self, key: str) -> None:
        self.log.append(("clear", key, None))

    async def sql_sync(self) -> None:
        self.log.append(("sync", "", None))

    def sql_send(self, service, handler, parameter, key, send_delay, idempotency_key, headers) -> None:
        self.log.append(("send", idempotency_key, (f"{service}/{key}/{handler}", parameter, send_delay)))

    def sql_attempt_end(self):
        return None

    def sql_on_attempt_end(self, listener: Callable[[], None]) -> Callable[[], None]:
        return lambda: None

    def kept(self, n: int | None = None) -> list[tuple[str, str, Any]]:
        """
        The commands a crash after the first n keeps: Restate keeps an
        attempt's commands in order, so a crash keeps a prefix of them.
        """
        return self.log[:n]

    def store(self, n: int | None = None) -> dict[str, bytes]:
        """The state once the commands a crash after the first n keeps are stored."""
        state = dict(self.state)
        for op, key, value in self.kept(n):
            if op == "set":
                state[key] = value
            elif op == "clear":
                state.pop(key, None)
        return state

    def sends(self, n: int | None = None) -> list[tuple[str, Any]]:
        return [(key, value) for op, key, value in self.kept(n) if op == "send"]


def fresh() -> str:
    """A namespace no session of this process has used: as another process sees the state."""
    return f"object-{next(_objects)}"


class _Stored:
    """A store over stored state, for reading it."""

    def __init__(self, state: dict[str, bytes]) -> None:
        self.state = state

    def get(self, key: str):
        future = asyncio.get_running_loop().create_future()
        future.set_result(self.state.get(key))
        return future

    def set(self, key: str, value: bytes) -> None:
        raise RuntimeError("read only")

    def clear(self, key: str) -> None:
        raise RuntimeError("read only")

    def sync(self) -> None:
        pass


async def outbox(state: dict[str, bytes]) -> list[Any]:
    """The transactions in the outbox of the database in ``state``."""
    db = await restate_sql.open(fresh(), _Stored(state))
    effects = await db.effects()
    await db.close()
    return json.loads(effects) if effects else []


async def run(host: FakeHost, body) -> Any:
    """One attempt of a handler: ``body(attempt)``, then the session ends with it."""
    attempt = SqlAttempt(host)
    try:
        result = await body(attempt)
    except BaseException:
        await attempt.end(failed=True)
        raise
    await attempt.end(failed=False)
    return result


async def test_statements_persist_and_a_later_attempt_reads_them():
    host = FakeHost({}, fresh())

    async def write(attempt: SqlAttempt):
        await attempt.db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")
        await attempt.db.execute("INSERT INTO t VALUES(?, ?)", [1, "one"])
        return await attempt.db.query_columns("SELECT * FROM t")

    assert await run(host, write) == (["id", "v"], [[1, "one"]])
    later = FakeHost(host.store(), fresh())
    assert await run(later, lambda a: a.db.query("SELECT v FROM t")) == [["one"]]


async def test_an_sql_error_is_terminal_and_leaves_the_session_usable():
    host = FakeHost({}, fresh())

    async def body(attempt: SqlAttempt):
        with pytest.raises(TerminalError, match="no such table") as error:
            await attempt.db.query("SELECT * FROM missing")
        assert isinstance(error.value.__cause__, RuntimeError)
        return await attempt.db.query("SELECT 1")

    assert await run(host, body) == [[1]]


async def test_a_storage_error_is_retried_not_terminal():
    host = FakeHost({}, fresh())
    host.fail_reads = True

    async def body(attempt: SqlAttempt):
        await attempt.db.query("SELECT 1")

    with pytest.raises(Exception) as error:
        await run(host, body)
    assert not isinstance(error.value, TerminalError)


async def test_a_transaction_rolls_back_when_its_body_raises_its_own_error():
    host = FakeHost({}, fresh())

    async def body(attempt: SqlAttempt):
        await attempt.db.execute("CREATE TABLE t(id INTEGER)")
        with pytest.raises(ValueError, match="changed my mind"):
            async with attempt.db.transaction() as db:
                await db.execute("INSERT INTO t VALUES(1)")
                raise ValueError("changed my mind")
        with pytest.raises(ValueError, match="again"):
            async with attempt.transaction() as t:
                await t.db.execute("INSERT INTO t VALUES(2)")
                t.tx.object_send(ship, "a", "apple")
                raise ValueError("again")
        return await attempt.db.query("SELECT count(*) FROM t")

    assert await run(host, body) == [[0]]
    assert host.sends() == []
    assert await outbox(host.store()) == []


async def test_a_transactions_sends_are_made_once_it_commits():
    host = FakeHost({}, fresh())

    async def body(attempt: SqlAttempt):
        await attempt.db.execute("CREATE TABLE t(id INTEGER)")
        async with attempt.transaction() as t:
            await t.db.execute("INSERT INTO t VALUES(1)")
            handle = t.tx.object_send(ship, "a", "apple", send_delay=timedelta(seconds=1))
            t.tx.generic_send("shop", "ship", b'"pear"', key="b", idempotency_key="mine")
            assert host.sends() == []
        with pytest.raises(TerminalError, match="no invocation id"):
            await handle.invocation_id()

    await run(host, body)
    [(first, (target, parameter, delay)), second] = host.sends()
    assert first.startswith("transaction/") and first.endswith("/0")
    assert (target, parameter, delay) == ("shop/a/ship", b'"apple"', timedelta(seconds=1))
    assert second == ("mine", ("shop/b/ship", b'"pear"', None))
    assert await outbox(host.store()) == []


async def test_a_send_after_the_transaction_has_ended_is_refused():
    host = FakeHost({}, fresh())

    async def body(attempt: SqlAttempt):
        async with attempt.transaction() as t:
            await t.db.execute("CREATE TABLE t(id INTEGER)")
        t.tx.object_send(ship, "a", "apple")

    with pytest.raises(TerminalError, match="the transaction has ended"):
        await run(host, body)


async def test_the_sends_of_a_commit_are_made_once_wherever_the_attempt_crashed():
    """
    For every point an attempt may crash at, keeping the commands before it,
    the next attempt makes the transaction's sends if, and only if, its
    commit landed: with the same keys, so that each is made once, and empties
    the outbox.
    """
    base = FakeHost({}, fresh())
    await run(base, lambda a: a.db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)"))
    stored = base.store()

    crashed = FakeHost(stored, fresh())
    attempt = SqlAttempt(crashed)
    async with attempt.transaction() as t:
        await t.db.execute("INSERT INTO t VALUES(7)")
        t.tx.object_send(ship, "a", "apple")
    await attempt.end(failed=False)
    assert len(crashed.sends()) == 1

    for n in range(len(crashed.log) + 1):
        after = FakeHost(crashed.store(n), fresh())

        async def recover(attempt: SqlAttempt):
            return await attempt.db.query("SELECT count(*) FROM t")

        [[rows]] = await run(after, recover)
        made = {key for key, _ in crashed.sends(n) + after.sends()}
        assert len(made) == rows, f"{n} commands: {rows} rows, sends made with keys {made}"
        assert await outbox(after.store()) == [], f"{n} commands"


async def test_the_outbox_holds_the_transactions_sends_until_they_are_stored():
    host = FakeHost({}, fresh())

    async def body(attempt: SqlAttempt):
        async with attempt.transaction() as t:
            await t.db.execute("CREATE TABLE t(id INTEGER)")
            t.tx.object_send(ship, "a", "apple")
        [outgoing] = await outbox(host.store())
        assert outgoing["sends"][0]["service"] == "shop"
        assert outgoing["sends"][0]["key"] == "a"

    await run(host, body)
    assert await outbox(host.store()) == []


async def test_ctx_db_is_for_the_handlers_of_objects():
    with pytest.raises(TerminalError, match="the handlers of objects"):
        await run(FakeHost({}, fresh(), kind=None), lambda a: a.db.query("SELECT 1"))


async def test_a_shared_handler_reads_the_stored_database_and_writes_nothing():
    writer = FakeHost({}, fresh())

    async def write(attempt: SqlAttempt) -> None:
        await attempt.db.execute("CREATE TABLE t(n INTEGER)")
        await attempt.db.execute("INSERT INTO t VALUES (1)")

    await run(writer, write)
    reader = FakeHost(writer.store(), fresh(), kind="shared")
    assert await run(reader, lambda a: a.db.query("SELECT n FROM t")) == [[1]]
    assert reader.log == []
    with pytest.raises(TerminalError, match="write statement in query_only mode"):
        await run(FakeHost(writer.store(), fresh(), kind="shared"), lambda a: a.db.execute("INSERT INTO t VALUES (2)"))


async def test_a_shared_handler_that_read_beside_a_commit_is_retried():
    writer = FakeHost({}, fresh())
    await run(writer, lambda a: a.db.execute("CREATE TABLE t(n INTEGER)"))
    state = writer.store()
    reader = FakeHost(state, fresh(), kind="shared")
    attempt = SqlAttempt(reader)
    assert await attempt.db.query("SELECT count(*) FROM t") == [[0]]
    # An exclusive handler commits meanwhile, to the same stored state.
    again = FakeHost(state, fresh())
    await run(again, lambda a: a.db.execute("INSERT INTO t VALUES (1)"))
    state.update(again.store())
    with pytest.raises(RuntimeError, match="changed while this shared handler read it"):
        await attempt.end(failed=False)


async def test_the_session_is_closed_once_the_handler_ends():
    host = FakeHost({}, fresh())
    attempt = SqlAttempt(host)
    await attempt.db.query("SELECT 1")
    await attempt.end(failed=False)
    with pytest.raises(RuntimeError, match="the handler has ended"):
        await attempt.db.query("SELECT 1")

"""
``ctx.step``, without a Restate server: attempts over an object's state in
memory (test_session's ``FakeHost``). Attempts with the same invocation id are
one invocation's; a crash is an attempt abandoned after a step, whose commands
are stored.
"""

from __future__ import annotations

import asyncio

import pytest
from restate.exceptions import TerminalError
from restate_fns.session import SqlAttempt, SqlTransaction
from tests.test_session import FakeHost, fresh, run, ship

pytestmark = pytest.mark.anyio


@pytest.fixture
def anyio_backend():
    return "asyncio"


class Model:
    """A request to a model, counted."""

    def __init__(self) -> None:
        self.calls = 0

    async def ask(self, prompt: str) -> str:
        self.calls += 1
        return f"answer {self.calls} to {prompt}"


async def never() -> None:
    raise AssertionError("ran again")


async def crashed(state: dict[str, bytes], body, invocation: str = "inv") -> dict[str, bytes]:
    """An attempt that runs ``body`` and crashes: the state it stored."""
    host = FakeHost(state, fresh(), invocation=invocation)
    await body(SqlAttempt(host))
    return host.store()


async def has_steps(state: dict[str, bytes]) -> bool:
    host = FakeHost(state, fresh(), invocation="look")
    rows = await run(host, lambda a: a.db.query("SELECT 1 FROM sqlite_schema WHERE name = 'restate_steps'"))
    return bool(rows)


async def test_a_step_runs_once_per_invocation():
    model = Model()
    state = await crashed({}, lambda a: a.step("ask", lambda: model.ask("hi")))
    retried = FakeHost(state, fresh())
    assert await run(retried, lambda a: a.step("ask", lambda: model.ask("hi"))) == "answer 1 to hi"
    assert model.calls == 1


async def test_a_step_commits_its_sql_and_sends_with_its_result_or_none_of_them():
    async def order(t: SqlTransaction, fail: bool) -> int:
        await t.db.execute("CREATE TABLE IF NOT EXISTS orders (item TEXT)")
        await t.db.execute("INSERT INTO orders VALUES ('book')")
        t.tx.object_send(ship, key="a", arg="book")
        if fail:
            raise ValueError("no")
        return 7

    async def failing_then_committing(a: SqlAttempt) -> None:
        with pytest.raises(ValueError, match="no"):
            await a.step("order", lambda t: order(t, True))
        # Not committed: the name is free, and nothing was stored.
        assert await a.step("order", lambda t: order(t, False)) == 7

    first = FakeHost({}, fresh())
    await failing_then_committing(SqlAttempt(first))
    assert len(first.sends()) == 1
    retried = FakeHost(first.store(), fresh())

    async def again(a: SqlAttempt) -> list:
        assert await a.step("order", never) == 7
        return await a.db.query("SELECT item FROM orders")

    assert await run(retried, again) == [["book"]]
    # The send of the commit, made again with its key from the outbox: Restate drops it.
    assert [key for key, _ in retried.sends()] == [key for key, _ in first.sends()]


async def test_steps_are_kept_until_the_invocation_ends_then_their_table_is_dropped():
    first = FakeHost({}, fresh())

    async def retryable(a: SqlAttempt) -> None:
        await a.step("ask", lambda: asyncio.sleep(0, 1))
        raise ConnectionError("later")

    with pytest.raises(ConnectionError):
        await run(first, retryable)
    assert await has_steps(first.store())

    second = FakeHost(first.store(), fresh())

    async def terminal(a: SqlAttempt) -> None:
        assert await a.step("ask", never) == 1
        raise TerminalError("done")

    with pytest.raises(TerminalError):
        await run(second, terminal)
    assert not await has_steps(second.store())


async def test_a_step_is_its_invocations_own_and_a_killed_ones_are_dropped():
    state = await crashed({}, lambda a: a.step("ask", lambda: asyncio.sleep(0, "first")), invocation="first")
    second = FakeHost(state, fresh(), invocation="second")
    assert await run(second, lambda a: a.step("ask", lambda: asyncio.sleep(0, "second"))) == "second"
    assert not await has_steps(second.store())


async def test_steps_without_sql_run_concurrently():
    both = asyncio.Event()
    started = 0

    async def body(name: str) -> str:
        nonlocal started
        started += 1
        if started == 2:
            both.set()
        await both.wait()  # each waits for the other to start
        return name

    async def concurrently(a: SqlAttempt) -> list:
        return list(await asyncio.gather(a.step("a", lambda: body("a")), a.step("b", lambda: body("b"))))

    assert await run(FakeHost({}, fresh()), concurrently) == ["a", "b"]


async def test_a_step_keeps_none_and_json():
    async def steps(a: SqlAttempt) -> None:
        await a.step("none", lambda: asyncio.sleep(0, None))
        await a.step("object", lambda: asyncio.sleep(0, {"text": "hi", "n": [1, 2]}))

    state = await crashed({}, steps)

    async def again(a: SqlAttempt) -> list:
        return [await a.step("none", never), await a.step("object", never)]

    assert await run(FakeHost(state, fresh()), again) == [None, {"text": "hi", "n": [1, 2]}]


async def test_a_step_refuses_a_name_used_the_session_inside_it_and_a_shared_handler():
    async def twice(a: SqlAttempt) -> None:
        await a.step("ask", lambda: asyncio.sleep(0, 1))
        with pytest.raises(TerminalError, match="has a step 'ask' already"):
            await a.step("ask", lambda: asyncio.sleep(0, 2))

    await run(FakeHost({}, fresh()), twice)

    async def nested(a: SqlAttempt) -> None:
        async def outer(t: SqlTransaction) -> None:
            await t.db.execute("CREATE TABLE t (v INTEGER)")
            with pytest.raises(TerminalError, match="inside a transaction"):
                await a.db.query("SELECT 1")
            with pytest.raises(TerminalError, match="inside a transaction"):
                await a.step("inner", lambda: asyncio.sleep(0, 1))

        await a.step("outer", outer)

    await run(FakeHost({}, fresh()), nested)

    reader = FakeHost({}, fresh(), kind="shared")
    with pytest.raises(TerminalError, match="ctx.step is for exclusive handlers"):
        await run(reader, lambda a: a.step("ask", lambda: asyncio.sleep(0, 1)))

"""The binding over an in-memory ordered store: SQL, values, and lost writes."""

import asyncio
import itertools

import pytest
import sqlite_kv


class Backend:
    """Stored values, and writes acknowledged but not yet confirmed."""

    def __init__(self):
        self.stored = {}
        self.unconfirmed = []

    def confirm(self):
        for key, value in self.unconfirmed:
            if value is None:
                self.stored.pop(key, None)
            else:
                self.stored[key] = value
        self.unconfirmed.clear()

    def lose(self):
        self.unconfirmed.clear()

    def request(self, fails=False):
        """A request's view: its own writes over the stored values."""
        return Request(self, fails)


class Request:
    def __init__(self, backend, fails):
        self.backend = backend
        self.fails = fails

    def get(self, key):
        # The read is issued now: what it sees is decided here.
        for k, value in reversed(self.backend.unconfirmed):
            if k == key:
                return value
        return self.backend.stored.get(key)

    def set(self, key, value):
        self.backend.unconfirmed.append((key, bytes(value)))

    def clear(self, key):
        self.backend.unconfirmed.append((key, None))

    async def sync(self):
        if self.fails:
            self.backend.lose()
            raise RuntimeError("the request failed")
        self.backend.confirm()


_namespaces = itertools.count()


def fresh():
    return f"namespace-{next(_namespaces)}"


async def test_runs_sql_and_round_trips_values():
    backend = Backend()
    ns = fresh()
    db = await sqlite_kv.open(ns, backend.request())
    await db.execute("CREATE TABLE t(i INTEGER, r REAL, s TEXT, b BLOB, n INTEGER)")
    await db.execute("INSERT INTO t VALUES(?, ?, ?, ?, ?)", [42, 1.5, "héllo", b"\x01\x02\x03", None])
    await db.execute("INSERT INTO t VALUES(?, ?, ?, ?, ?)", [2**62, -0.25, "", bytearray(), True])
    rows = await db.query("SELECT * FROM t ORDER BY i")
    assert rows == [[42, 1.5, "héllo", b"\x01\x02\x03", None], [2**62, -0.25, "", b"", 1]]
    columns, rows = await db.query_columns("SELECT i AS answer FROM t WHERE i = ?", [42])
    assert (columns, rows) == (["answer"], [[42]])
    await db.save()

    # Another session, and another process's view (no cached state), read it.
    for name in [ns, fresh()]:
        again = await sqlite_kv.open(name, backend.request())
        assert await again.query("SELECT count(*) FROM t") == [[2]]
        await again.save()


async def test_rejects_values_it_cannot_store():
    db = await sqlite_kv.open(fresh(), Backend().request())
    with pytest.raises(ValueError, match="exceeds 64 bits"):
        await db.query("SELECT ?", [2**64])
    with pytest.raises(TypeError, match="unsupported SQL value"):
        await db.query("SELECT ?", [object()])  # type: ignore[list-item]
    with pytest.raises(TypeError, match="a sequence of values"):
        await db.query("SELECT ?", "x")
    await db.save()


async def test_an_sql_error_leaves_the_session_usable_and_a_transaction_rolls_back():
    db = await sqlite_kv.open(fresh(), Backend().request())
    await db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)")
    with pytest.raises(RuntimeError, match="no such table"):
        await db.query("SELECT * FROM missing")
    assert db.is_usable()
    with pytest.raises(ValueError, match="abort"):
        async with db.transaction() as tx:
            await tx.execute("INSERT INTO t VALUES(1)")
            raise ValueError("abort")
    assert await db.query("SELECT count(*) FROM t") == [[0]]
    async with db.transaction() as tx:
        assert await tx.query("INSERT INTO t VALUES(2) RETURNING id") == [[2]]
    assert await db.query("SELECT id FROM t") == [[2]]
    await db.save()


async def consistent_after_loss(lost, close_first):
    """Requests insert a row each and save; request ``lost`` fails and loses its
    writes, its confirmation failing or the session closed first. The owner and
    a process without cached state must then read exactly the stored rows."""
    backend = Backend()
    ns = fresh()
    db = await sqlite_kv.open(ns, backend.request())
    await db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v BLOB)")
    await db.save()
    expected = 0
    for id in range(20):
        fails = id == lost
        db = await sqlite_kv.open(ns, backend.request(fails))
        await db.execute("INSERT INTO t VALUES(?, randomblob(3000))", [id])
        if not fails:
            await db.save()
            expected += 1
        elif close_first:
            await db.close()
            backend.lose()
        else:
            with pytest.raises(RuntimeError, match="the request failed"):
                await db.save()
    for name in [ns, fresh()]:
        view = await sqlite_kv.open(name, backend.request())
        [[rows]] = await view.query("SELECT count(*) FROM t")
        [[integrity]] = await view.query("PRAGMA integrity_check")
        await view.save()
        if rows != expected or integrity != "ok":
            return False
    return True


async def test_lost_writes_leave_every_reader_consistent():
    for close_first in [False, True]:
        for lost in range(1, 15):
            assert await consistent_after_loss(lost, close_first), f"lost request {lost}, closed first: {close_first}"


async def test_a_write_the_store_rejects_is_reported_by_the_next_sync():
    store = Backend().request()

    def rejected(key, value):
        raise RuntimeError("state is closed")

    store.set = rejected
    db = await sqlite_kv.open(fresh(), store)
    await db.execute("CREATE TABLE t(id INTEGER)")
    with pytest.raises(RuntimeError, match="state is closed"):
        await db.save()


async def test_a_transaction_holds_the_session_other_calls_wait_until_it_ends():
    db = await sqlite_kv.open(fresh(), Backend().request())
    await db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)")
    handle = None

    async def rolled_back():
        nonlocal handle
        async with db.transaction() as tx:
            handle = tx
            await tx.execute("INSERT INTO t VALUES(1)")
            await asyncio.sleep(0.01)
            raise ValueError("abort")

    async def committed():
        async with db.transaction() as tx:
            await tx.execute("INSERT INTO t VALUES(3)")

    first = asyncio.ensure_future(rolled_back())
    await asyncio.sleep(0)
    # Started while the transaction waits: neither runs inside it.
    write = asyncio.ensure_future(db.execute("INSERT INTO t VALUES(2)"))
    last = asyncio.ensure_future(committed())
    with pytest.raises(ValueError, match="abort"):
        await first
    await write
    await last
    assert await db.query("SELECT id FROM t ORDER BY id") == [[2], [3]]
    assert handle is not None
    with pytest.raises(RuntimeError, match="the transaction has ended"):
        await handle.query("SELECT 1")
    await db.save()


async def test_a_commit_that_fails_rolls_the_transaction_back():
    db = await sqlite_kv.open(fresh(), Backend().request())
    await db.execute("PRAGMA foreign_keys=ON")
    await db.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)")
    await db.execute("CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)")
    # The deferred constraint fails at COMMIT, not at the INSERT.
    with pytest.raises(RuntimeError, match="FOREIGN KEY constraint failed"):
        async with db.transaction() as tx:
            await tx.execute("INSERT INTO c VALUES(7)")
    assert db.is_usable()
    # Nothing of it is left, and the next transaction can begin.
    assert await db.query("SELECT * FROM c") == []
    async with db.transaction() as tx:
        await tx.execute("INSERT INTO p VALUES(7)")
    assert await db.query("SELECT id FROM p") == [[7]]
    await db.save()


async def test_a_call_running_meanwhile_does_not_make_the_session_look_unusable():
    db = await sqlite_kv.open(fresh(), Backend().request())
    await db.execute("CREATE TABLE t(id INTEGER)")
    for _ in range(50):
        # The body fails while version() holds the session: the rollback still runs.
        with pytest.raises(ValueError, match="no"):
            async with db.transaction() as tx:
                await tx.execute("INSERT INTO t VALUES(1)")
                versions = [asyncio.ensure_future(db.version()) for _ in range(4)]
                raise ValueError("no")
        await asyncio.gather(*versions)
    assert await db.query("SELECT count(*) FROM t") == [[0]]
    # Nor while a statement runs.
    running = asyncio.ensure_future(
        db.query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000) SELECT count(*) FROM n"
        )
    )
    await asyncio.sleep(0)
    assert db.is_usable()
    await running
    await db.save()
    assert not db.is_usable()


async def test_the_version_grows_with_each_commit_and_only_then():
    backend = Backend()
    namespace = fresh()
    db = await sqlite_kv.open(namespace, backend.request())
    v0 = await db.version()
    await db.execute("CREATE TABLE t(id INTEGER)")
    v1 = await db.version()
    assert v1 > v0
    await db.query("SELECT * FROM t")
    assert await db.version() == v1
    async with db.transaction() as tx:
        await tx.execute("INSERT INTO t VALUES(1)")
        inside = await db.version()
    assert inside == v1
    v2 = await db.version()
    assert v2 > v1
    with pytest.raises(ValueError, match="no"):
        async with db.transaction() as tx:
            await tx.execute("INSERT INTO t VALUES(2)")
            raise ValueError("no")
    assert await db.version() == v2
    # A commit that changes nothing grows it only when recorded.
    async with db.transaction() as tx:
        await tx.query("SELECT * FROM t")
    assert await db.version() == v2
    async with db.transaction() as tx:
        await tx.execute("UPDATE t SET id = 5 WHERE id = 99")
        tx.record_commit()
    v3 = await db.version()
    assert v3 > v2
    await db.save()
    # A later session opens at the same version.
    again = await sqlite_kv.open(namespace, backend.request())
    assert await again.version() == v3
    await again.save()


async def test_a_call_to_the_session_inside_its_transaction_is_refused_not_left_waiting():
    db = await sqlite_kv.open(fresh(), Backend().request())
    await db.execute("CREATE TABLE t(id INTEGER)")

    async def after(delay):
        await asyncio.sleep(delay)
        await db.execute("INSERT INTO t VALUES(3)")

    async with db.transaction() as tx:
        await tx.execute("INSERT INTO t VALUES(1)")
        # It would wait for this transaction, which waits for this body.
        with pytest.raises(RuntimeError, match="use the transaction handle"):
            await db.execute("INSERT INTO t VALUES(2)")
        with pytest.raises(RuntimeError, match="use the transaction handle"):
            async with db.transaction():
                pass
        # Once the body has ended, a task it started may use the session.
        later = asyncio.ensure_future(after(0.005))
    await later
    assert await db.query("SELECT id FROM t ORDER BY id") == [[1], [3]]
    await db.save()


async def test_a_call_to_a_session_inside_a_transaction_of_another_inside_its_own_is_refused():
    a = await sqlite_kv.open(fresh(), Backend().request())
    b = await sqlite_kv.open(fresh(), Backend().request())
    async with a.transaction():
        async with b.transaction() as tx:
            await tx.query("SELECT 1")
            # `a` waits for its transaction, which waits for this body.
            with pytest.raises(RuntimeError, match="use the transaction handle"):
                await a.query("SELECT 1")
        # Once b's transaction has ended, b may be used again from a's body.
        assert await b.query("SELECT 1") == [[1]]
    await a.save()
    await b.save()


async def test_a_statement_completes_after_its_writes_reached_the_store_however_many():
    backend = Backend()
    db = await sqlite_kv.open(fresh(), backend.request())
    await db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, b BLOB)")
    # Thousands of page writes, each scheduled on the loop.
    await db.execute(
        "INSERT INTO t(b) SELECT randomblob(3000) FROM "
        "(WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000) SELECT x FROM c)"
    )
    at_completion = len(backend.unconfirmed)
    await asyncio.sleep(0.05)
    assert at_completion > 1000
    assert at_completion == len(backend.unconfirmed)
    await db.save()


async def test_an_awaitable_get_is_read_when_issued():
    """A store whose get returns an awaitable, as a Restate context's does."""
    backend = Backend()
    plain = backend.request()

    class AwaitableGets(Request):
        def get(self, key):
            future = asyncio.get_running_loop().create_future()
            future.set_result(plain.get(key))
            return future

    ns = fresh()
    db = await sqlite_kv.open(ns, AwaitableGets(backend, False))
    await db.execute("CREATE TABLE t(id INTEGER)")
    await db.execute("INSERT INTO t VALUES(1)")
    await db.save()
    for name in [ns, fresh()]:
        again = await sqlite_kv.open(name, AwaitableGets(backend, False))
        assert await again.query("SELECT * FROM t") == [[1]]
        await again.save()

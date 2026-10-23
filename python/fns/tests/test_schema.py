"""An object's schema (schema.py): its steps, applied once per key, by the handlers that open its database."""

import itertools

import pytest
from restate.exceptions import TerminalError
from restate_fns import Schema
from restate_fns import schema as schemas
from test_session import FakeHost, run

pytestmark = [pytest.mark.anyio]

_names = itertools.count()


def object_with(*steps) -> str:
    """A new object's name, its schema ``steps``."""
    name = f"schema-{next(_names)}"
    schemas.register(name, steps)
    return name


async def test_the_objects_schema_is_applied_once_where_a_key_is_first_opened():
    name = object_with("CREATE TABLE notes (text TEXT NOT NULL)")
    host = FakeHost({}, f"{name}/alice")
    await run(host, lambda a: a.db.execute("INSERT INTO notes VALUES ('milk')"))
    later = FakeHost(host.store(), f"{name}/alice")
    assert await run(later, lambda a: a.db.query("SELECT text FROM notes")) == [["milk"]]
    assert await run(later, lambda a: a.db.query("SELECT name, version FROM restate_schema")) == [[name, 1]]


async def test_a_step_added_later_runs_on_each_existing_database_once():
    name = object_with("CREATE TABLE notes (text TEXT NOT NULL)")
    host = FakeHost({}, f"{name}/alice")
    await run(host, lambda a: a.db.execute("INSERT INTO notes VALUES ('milk')"))

    async def done(tx):  # a step of data: the notes so far are done
        await tx.execute("UPDATE notes SET done = 1")

    schemas.register(name, ["CREATE TABLE notes (text TEXT NOT NULL)", "ALTER TABLE notes ADD COLUMN done INT", done])
    later = FakeHost(host.store(), f"{name}/alice")
    assert await run(later, lambda a: a.db.query("SELECT text, done FROM notes")) == [["milk", 1]]
    again = FakeHost(later.store(), f"{name}/alice")
    await run(again, lambda a: a.db.query("SELECT 1"))
    assert [op for op, _, _ in again.log if op == "set"] == []  # nothing to apply: nothing written


async def test_a_shared_handler_reads_a_key_never_written_as_empty_tables_and_stores_nothing():
    name = object_with("CREATE TABLE notes (text TEXT NOT NULL)")
    reader = FakeHost({}, f"{name}/nobody", kind="shared")
    assert await run(reader, lambda a: a.db.query("SELECT text FROM notes")) == []
    assert reader.log == []
    with pytest.raises(TerminalError, match="query_only"):
        await run(FakeHost({}, f"{name}/nobody", kind="shared"), lambda a: a.db.execute("INSERT INTO notes VALUES (1)"))


LIBRARY = Schema("library", ["CREATE TABLE library_items (item TEXT)"])


async def test_a_librarys_schema_is_ensured_where_it_is_used_beside_the_objects():
    name = object_with("CREATE TABLE notes (text TEXT NOT NULL)")
    host = FakeHost({}, f"{name}/alice")

    async def use(attempt):
        await LIBRARY.ensure(attempt.db)
        await attempt.db.execute("INSERT INTO library_items VALUES ('a')")
        return await attempt.db.query("SELECT name, version FROM restate_schema ORDER BY name")

    assert await run(host, use) == [["library", 1], [name, 1]]
    # In a shared handler, the library's tables are its own: empty, and stored nowhere.
    reader = FakeHost({}, f"{name}/bob", kind="shared")

    async def read(attempt):
        await LIBRARY.ensure(attempt.db)
        return await attempt.db.query("SELECT item FROM library_items")

    assert await run(reader, read) == []
    assert reader.log == []

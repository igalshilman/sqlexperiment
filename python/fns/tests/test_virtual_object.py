"""restate_fns.VirtualObject: its options, and which of its handlers get ctx.db."""

from datetime import timedelta

import restate
import restate_fns
from restate.handler import handler_from_callable

notes = restate_fns.VirtualObject("notes")


@notes.handler()
async def add(ctx: restate_fns.ObjectContext, note: str) -> None:
    await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])


@notes.handler(kind="shared")
async def count(ctx: restate_fns.ObjectSharedContext) -> int:
    [[n]] = await ctx.db.query("SELECT count(*) FROM notes")
    return n


@notes.handler(journal_replay=True)
async def wait(ctx: restate_fns.JournaledObjectContext) -> str:
    _, signal = ctx.awakeable()
    return await signal


class Context:
    """A test's context, with a ctx.db of its own."""

    def __init__(self) -> None:
        self.db = Database()


class Database:
    def __init__(self) -> None:
        self.statements: list[str] = []

    async def execute(self, sql: str, params=None) -> None:
        self.statements.append(sql)

    async def query(self, sql: str, params=None):
        return [[len(self.statements)]]


def test_it_is_an_object_without_journal_replay_with_lazy_state_and_no_journal_kept():
    assert isinstance(notes, restate.VirtualObject)
    assert notes.enable_lazy_state
    assert notes.journal_retention == timedelta(0)
    assert not notes.handlers["add"].journal_replay
    assert not notes.handlers["count"].journal_replay
    assert notes.handlers["wait"].journal_replay


def test_options_add_to_the_sql_options():
    obj = restate_fns.VirtualObject("o", inactivity_timeout=timedelta(seconds=5), enable_lazy_state=False)

    @obj.handler()
    async def h(ctx: restate_fns.ObjectContext) -> None:
        pass

    assert obj.inactivity_timeout == timedelta(seconds=5)
    assert not obj.enable_lazy_state
    assert not obj.handlers["h"].journal_replay


def test_a_handler_keeps_its_signature():
    h = handler_from_callable(add)
    assert h.arity == 2
    assert h.handler_io.input_type is not None


async def test_a_context_with_a_db_of_its_own_is_passed_through():
    ctx = Context()
    await add(ctx, "milk")
    assert ctx.db.statements == ["INSERT INTO notes (text) VALUES (?)"]
    assert await count(ctx) == 1


def test_a_journaled_handler_is_the_sdks_own():
    # The SDK's wrapper, around the function as written: no ctx.db.
    assert not hasattr(wait.__wrapped__, "__wrapped__")
    assert add.__wrapped__.__wrapped__.__name__ == "add"

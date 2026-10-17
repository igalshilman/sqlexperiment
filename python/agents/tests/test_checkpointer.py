"""SqlCheckpointSaver: LangGraph's checkpointer conformance suite, over an in-memory store."""

from langgraph.checkpoint.conformance import checkpointer_test, validate
from memory_store import session
from restate_agents.langgraph import SqlCheckpointSaver


@checkpointer_test(name="SqlCheckpointSaver")
async def saver():
    db = await session()
    yield SqlCheckpointSaver(db)
    await db.close()


async def test_conformance():
    report = await validate(saver)
    print(report)
    assert report.passed_all_base(), report

"""
An assistant with a memory, on LangGraph: the facts it learns about you go
into a table of its own SQL database, next to its conversation. Its tools
write and read them through ``ctx.db``, and handlers of your own serve them,
for a profile page say.

    ANTHROPIC_API_KEY=... uv run examples/memory.py   # in python/, after make py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
    curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
    curl localhost:8080/assistant/alice/facts
"""

from langchain_core.tools import tool
from restate import ObjectContext
from restate_agents import context, langgraph, serve

SCHEMA = "CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)"


@tool
async def remember(fact: str) -> str:
    """Remember a fact about the user, for this conversation and later ones."""
    db = context().db
    await db.execute(SCHEMA)
    # Once, however often a crash runs the tool again.
    await db.execute("INSERT OR IGNORE INTO facts (fact) VALUES (?)", [fact])
    return f"Remembered: {fact}"


@tool
async def recall() -> list[str]:
    """Everything remembered about the user."""
    db = context().db
    await db.execute(SCHEMA)
    return [fact for [fact] in await db.query("SELECT fact FROM facts ORDER BY rowid")]


assistant = langgraph.agent(
    "assistant",
    system="You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.",
    tools=[remember, recall],
)


# Handlers of your own, beside the agent's chat and messages.
@assistant.handler()
async def facts(ctx: ObjectContext) -> list[str]:
    """What the assistant knows about the user."""
    await ctx.db.execute(SCHEMA)
    return [fact for [fact] in await ctx.db.query("SELECT fact FROM facts ORDER BY rowid")]


@assistant.handler()
async def forget(ctx: ObjectContext, fact: str) -> None:
    """Forget a fact."""
    await ctx.db.execute(SCHEMA)
    await ctx.db.execute("DELETE FROM facts WHERE fact = ?", [fact])


if __name__ == "__main__":
    serve(assistant)

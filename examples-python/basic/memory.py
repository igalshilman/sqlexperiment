"""
An assistant with a memory: the facts it learns about you go into a table
of its own SQL database, next to its conversation, through its tools; and a
handler of your own serves them, for a profile page say.

    ANTHROPIC_API_KEY=... uv run memory.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
    curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
    curl localhost:8080/assistant/alice/facts
"""

from restate_agents import has_table, langgraph, serve, tool


async def facts_table(db):
    await db.execute("CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)")
    return db


@tool
async def remember(ctx, fact: str) -> str:
    """Remember a fact about the user, for this conversation and later ones."""
    db = await facts_table(ctx.db)
    await db.execute("INSERT OR IGNORE INTO facts VALUES (?)", [fact])
    return f"Remembered: {fact}"


@tool
async def recall(ctx) -> list[str]:
    """Everything remembered about the user."""
    db = await facts_table(ctx.db)
    return [fact for (fact,) in await db.query("SELECT fact FROM facts ORDER BY rowid")]


assistant = langgraph.agent(
    "assistant",
    system="Remember what you learn about the user, and recall it when it helps.",
    tools=[remember, recall],
)


@assistant.handler(kind="shared")
async def facts(ctx) -> list[str]:
    """What the assistant remembers about the user: shared, so it answers while a message is answered."""
    if not await has_table(ctx.db, "facts"):
        return []
    return [fact for (fact,) in await ctx.db.query("SELECT fact FROM facts ORDER BY rowid")]


if __name__ == "__main__":
    serve(assistant)

"""
An assistant with a memory: the facts it learns about you go into a table
of its own SQL database, next to its conversation, through its tools; and
handlers of your own serve them, for a profile page say.

    ANTHROPIC_API_KEY=... uv run memory.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
    curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
    curl localhost:8080/assistant/alice/facts
"""

from restate_agents import langgraph, serve, tool


@tool
async def remember(ctx, fact: str) -> str:
    """Remember a fact about the user, for this conversation and later ones."""
    await ctx.db.execute("INSERT OR IGNORE INTO facts VALUES (?)", [fact])
    return f"Remembered: {fact}"


@tool
async def recall(ctx) -> list[str]:
    """Everything remembered about the user."""
    return [fact for (fact,) in await ctx.db.query("SELECT fact FROM facts")]


assistant = langgraph.agent(
    "assistant",
    schema="CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)",
    system="Remember what you learn about the user, and recall it when it helps.",
    tools=[remember, recall],
)


@assistant.handler
async def facts(ctx) -> list[str]:
    return [fact for (fact,) in await ctx.db.query("SELECT fact FROM facts")]


@assistant.handler
async def forget(ctx, fact: str):
    await ctx.db.execute("DELETE FROM facts WHERE fact = ?", [fact])


if __name__ == "__main__":
    serve(assistant)

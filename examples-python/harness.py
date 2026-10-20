"""
An assistant each user sets up as it runs: each user (the object's key)
picks its model and persona, kept in the object's database, which the agent
reads for each message; ``compact`` has the model summarize the conversation
so far, to keep its context short; and ``reset`` starts it over, with a note
to carry on.

    ANTHROPIC_API_KEY=... uv run harness.py
    restate deployments register http://localhost:9080
    curl localhost:8080/assistant/alice/configure --json '{"persona": "You answer as a pirate."}'
    curl localhost:8080/assistant/alice/chat -d 'What time is it?'
    curl localhost:8080/assistant/alice/compact -d 'Keep the times you were asked'
    curl localhost:8080/assistant/alice/reset -d 'Alice likes short answers'
    curl localhost:8080/assistant/alice/messages

The model is one of Claude's, such as ``{"model": "anthropic/claude-haiku-5-5"}``.
"""

import json
from datetime import UTC, datetime

from restate_agents import claude, serve, tool


async def settings_of(ctx) -> dict:
    """What the user set up: ``model`` and ``persona``, each if set."""
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 0), json TEXT)")
    rows = await ctx.db.query("SELECT json FROM settings")
    return json.loads(rows[0][0]) if rows else {}


async def model(ctx) -> str | None:
    return (await settings_of(ctx)).get("model")


async def persona(ctx) -> str:
    return (await settings_of(ctx)).get("persona", "You are a helpful assistant.")


@tool
def clock() -> str:
    """The current time, in UTC."""
    return datetime.now(UTC).isoformat()


assistant = claude.agent("assistant", model=model, system=persona, tools=[clock])


@assistant.handler
async def configure(ctx, model: str | None = None, persona: str | None = None) -> dict:
    """Set the model or the persona (or both) for the next messages."""
    settings = await settings_of(ctx)
    settings.update({k: v for k, v in {"model": model, "persona": persona}.items() if v is not None})
    await ctx.db.execute("INSERT OR REPLACE INTO settings VALUES (0, ?)", [json.dumps(settings)])
    return settings


@assistant.handler(message=True)
async def compact(ctx, instructions: str) -> str:
    """Summarize the conversation so far, as ``instructions`` say, in place of it."""
    return await claude.compact(ctx, instructions, model=await model(ctx))


@assistant.handler(message=True)
async def reset(ctx, handoff: str) -> None:
    """Start the conversation over, with ``handoff`` as what to carry on with."""
    await claude.reset(ctx, handoff)


if __name__ == "__main__":
    serve(assistant)

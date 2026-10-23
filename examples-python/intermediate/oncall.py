"""
An on-call responder on Google ADK, its steps Restate's: for each incident
(a key) it checks the services' health with a Restate service (ops.py), keeps the
incident's timeline in its own SQL table, pages the engineer on call and
waits for them to acknowledge (a signal, without holding a process), and
schedules the post-incident review for the next day (a delayed send, which
Restate keeps until then). A handler of your own serves the timeline.

    GOOGLE_API_KEY=... uv run intermediate/oncall.py
    restate deployments register http://localhost:9080
    curl localhost:8080/oncall/inc-42/chat/send -d 'Checkout errors went up to 30% five minutes ago'

The responder pages the engineer and waits; acknowledge, as they would
from their pager, with what they found:

    curl localhost:8080/oncall/inc-42/pending
    curl localhost:8080/oncall/inc-42/signal --json '{"name": "ack", "value": "On it: rolling back payments 2.4.1"}'
    curl localhost:8080/oncall/inc-42/timeline        # the incident, step by step
    curl localhost:8080/oncall/inc-42/chat -d 'Error rate is back to normal'

The review shows in Restate's UI (localhost:9070) as a scheduled invocation
of ``reviews/schedule``; REVIEW_SECONDS=30 makes it come sooner.
"""

import os
from datetime import timedelta

from ops import health, monitoring, reviews, schedule
from pydantic import BaseModel
from restate_agents import adk, call, send, serve, signal, tool


class Page(BaseModel):
    """What the engineer on call is paged with."""

    severity: str
    summary: str


@tool
async def note(ctx, call_id, entry: str) -> str:
    """Add an entry to the incident's timeline: what was found, decided or done."""
    # Keyed by the tool call's id: a retried call notes it once.
    await ctx.db.execute("INSERT OR IGNORE INTO timeline VALUES (?, datetime('now'), ?)", [call_id, entry])
    return "noted"


responder = adk.agent(
    "oncall",
    model="google/gemini-2.5-flash",
    system=(
        "You respond to incidents. Check the health of checkout, payments and inventory, note what you find, "
        "page the engineer on call and wait for them, note what they say. Once the incident is resolved, "
        "schedule its review and note it."
    ),
    tools=[
        call(health),
        note,
        signal("ack", description="Page the engineer on call, and wait for them to acknowledge.", input=Page),
        send(
            schedule,
            name="schedule_review",
            description="Schedule the incident's review, for the next day.",
            delay=timedelta(seconds=int(os.environ.get("REVIEW_SECONDS", 24 * 60 * 60))),
        ),
    ],
    # The incident's timeline: the schema of each incident's database, beside the conversation.
    schema=["CREATE TABLE timeline (id TEXT PRIMARY KEY, at TEXT, entry TEXT)"],
)


@responder.handler(kind="shared")
async def timeline(ctx) -> list[str]:
    """The incident's timeline, oldest first."""
    return [f"{at} {entry}" for at, entry in await ctx.db.query("SELECT at, entry FROM timeline ORDER BY rowid")]


if __name__ == "__main__":
    serve(monitoring, reviews, responder)

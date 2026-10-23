"""
An agent that follows up later: it schedules a check-in with the customer a
day from now, a one-way call Restate keeps until then. Nothing runs in the
meantime, and a restart changes nothing.

    ANTHROPIC_API_KEY=... uv run followup.py
    restate deployments register http://localhost:9080
    curl localhost:8080/concierge/alice/chat -d 'My order 103 is late. Can you keep an eye on it?'

The check-in shows in Restate's UI (localhost:9070) as a scheduled
invocation of ``checkins/checkin``; FOLLOW_UP_SECONDS=10 makes it come sooner.
"""

import os
from datetime import timedelta

import restate_fns as restate
from pydantic import BaseModel
from restate_agents import claude, send, serve


class Checkin(BaseModel):
    customer: str
    note: str


checkins = restate.Service("checkins")


@checkins.handler()
async def checkin(ctx: restate.Context, checkin: Checkin) -> None:
    """Check in with a customer about a note."""
    print(f"Checking in with {checkin.customer}: {checkin.note}")  # a stand-in for an email or a message


concierge = claude.agent(
    "concierge",
    system="You are a concierge for an online shop. When a customer needs it, schedule a check-in.",
    tools=[
        send(
            checkin,
            name="schedule_checkin",
            description="Check in with the customer a day from now, with a note of what to check.",
            delay=timedelta(seconds=int(os.environ.get("FOLLOW_UP_SECONDS", 24 * 60 * 60))),
        )
    ],
)

if __name__ == "__main__":
    serve(checkins, concierge)

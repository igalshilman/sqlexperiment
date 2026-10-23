"""
The check-ins that followup.py's concierge schedules, as a Restate service:
the concierge sends to it with a delay, and Restate keeps the call until then.
"""

import restate_fns as restate
from pydantic import BaseModel


class Checkin(BaseModel):
    customer: str
    note: str


checkins = restate.Service("checkins")


@checkins.handler()
async def checkin(ctx: restate.Context, checkin: Checkin) -> None:
    """Check in with a customer about a note."""
    print(f"Checking in with {checkin.customer}: {checkin.note}")  # a stand-in for an email or a message

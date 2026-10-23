"""
The operations that oncall.py's responder works with, as Restate services:
monitoring, the services' health, and reviews, which schedules a
post-incident review (the responder sends to it with a delay).
"""

import restate_fns as restate
from pydantic import BaseModel

monitoring = restate.Service("monitoring")


@monitoring.handler()
async def health(ctx: restate.Context, service: str) -> str:
    """The health of a service (checkout, payments, inventory): its error rate and its last deploy."""
    if service == "payments":
        return "payments: 31% errors since 14:02; deployed 2.4.1 at 14:00"
    return f"{service}: healthy, 0.1% errors"


class Review(BaseModel):
    incident: str
    summary: str


reviews = restate.Service("reviews")


@reviews.handler()
async def schedule(ctx: restate.Context, review: Review) -> None:
    """Schedule the post-incident review of an incident."""
    print(f"Review of {review.incident}: {review.summary}")  # a stand-in for a calendar invite

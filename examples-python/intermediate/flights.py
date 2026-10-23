"""
The airline's systems that airline.py's agents work with, as Restate
services: flights, a plain service they look flights up with, and rebooking,
a workflow that charges a change's fee once, its steps journaled.
"""

import hashlib
from datetime import timedelta

import restate_fns as restate
from pydantic import BaseModel

flights = restate.Service("flights")


@flights.handler()
async def options(ctx: restate.Context, flight: str) -> str:
    """The status of a flight, and the flights it can be changed to, with their fees."""
    return (
        f"{flight} is delayed by 3 hours. Same route, same day: "
        "RA 100 at 07:10 (fee $40, 2 seats left), RA 104 at 18:30 (no fee, 9 seats left)."
    )


class Change(BaseModel):
    """A change of flight: the booking's flight, the new one, and its fee."""

    flight: str
    to: str
    fee: float


rebooking = restate.Workflow("rebooking")


@rebooking.main()
async def rebook(ctx: restate.WorkflowContext, change: Change) -> str:
    """Rebook a traveller onto another flight and charge its fee. Only once the traveller confirmed the fee."""
    await ctx.run_typed("hold the seat", lambda: f"seat held on {change.to}")
    if change.fee:
        await ctx.run_typed("charge the fee", lambda: f"charged ${change.fee}")  # a stand-in for the payments API
    await ctx.sleep(timedelta(seconds=2))  # the ticket is reissued
    ticket = hashlib.sha1(ctx.key().encode()).hexdigest()[:6].upper()  # the same in each attempt
    return f"Rebooked onto {change.to}, ticket {ticket}."

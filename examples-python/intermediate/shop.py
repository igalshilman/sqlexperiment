"""
The shop that support.py's agent works with, as Restate services: orders, a
plain service it looks orders up with, and refund, a workflow that runs to
its end once, however long it takes.
"""

from datetime import timedelta

import restate_fns as restate
from pydantic import BaseModel

orders = restate.Service("orders")


@orders.handler()
async def status(ctx: restate.Context, order: str) -> str:
    """The status of an order."""
    return f"Order {order} was delivered yesterday."


class Refund(BaseModel):
    order: str
    amount: float


refund = restate.Workflow("refund")


@refund.main()
async def run(ctx: restate.WorkflowContext, refund: Refund) -> str:
    """Refund an order. Only once a manager approved it."""
    await ctx.sleep(timedelta(seconds=5))  # a stand-in for the payment provider
    return f"Refunded ${refund.amount} for order {refund.order}."

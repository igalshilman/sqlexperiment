"""
Shipping a change, as a Restate workflow: its checks run in CI, then its
pull request opens, once per change (the workflow's key), however often it
is asked. CI and the repository are stand-ins: a wait, and a print.
"""

from datetime import timedelta

import restate_fns as restate
from pydantic import BaseModel, Field


class Change(BaseModel):
    title: str = Field(description="The pull request's title")
    summary: str = Field(description="What changed and why, for the people who review it")
    diff: str = Field(description="The change, as `git diff` shows it")


shipping = restate.Workflow("shipping")


@shipping.main()
async def ship(ctx: restate.WorkflowContext, change: Change) -> str:
    """Ship a change that the person approved: CI checks it, then its pull request opens. Its URL."""
    await ctx.sleep(timedelta(seconds=5))  # a stand-in for your CI
    await ctx.run_typed("open the pull request", lambda: print(f"{change.title}\n\n{change.summary}\n\n{change.diff}"))
    return f"https://example.com/pulls/{ctx.key()}"

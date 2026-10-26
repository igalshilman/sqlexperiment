"""
The supervisor's agents, on Pydantic AI, each answering typed: two
executors that propose a fix for a finding, a fast one and a careful one
(the next rung, when the fast one's fix is rejected), and a judge that
audits each fix before anything is done.

The executors look the service and the package up with Restate's tools
(``call``), so their calls are stored once and show in Restate's UI.
"""

from typing import Literal

from pydantic import BaseModel, Field
from pydantic_ai import Agent
from restate_agents import call, pydantic_ai
from systems import describe, lookup


class Finding(BaseModel):
    """A security finding of a service, as a scanner reports it."""

    service: str
    title: str
    severity: Literal["low", "medium", "high", "critical"]
    detail: str


class Fix(BaseModel):
    summary: str
    diff: str = Field(description="The change, as a unified diff of the service's dependency manifest or code.")
    risk: Literal["low", "medium", "high"] = Field(description="How likely the change breaks the service.")
    rollback: str = Field(description="How to undo the change.")


class Task(BaseModel):
    """What an executor works on: the finding, and why the fixes before were rejected."""

    finding: Finding
    rejected: list[str] = []


class Proposal(BaseModel):
    finding: Finding
    fix: Fix


class Verdict(BaseModel):
    accepted: bool
    reasons: str = Field(description="Why, specifically: what is wrong with the fix, or what makes it safe.")


TOOLS = pydantic_ai.tools([call(describe), call(lookup)])
EXECUTOR = (
    "You fix security findings: look the service and the vulnerable package up, then propose the smallest change "
    "that fixes the finding, with its risk and how to roll it back. Address every reason a fix before was rejected."
)


def _asked(task: Task) -> str:
    rejected = "".join(f"\nRejected before: {reason}" for reason in task.rejected)
    return f"Finding: {task.finding.model_dump_json()}{rejected}"


fast = Agent("openai:gpt-6-luna", instructions=EXECUTOR + " Be quick.", tools=TOOLS)
fast_executor = pydantic_ai.agent("fast_executor", fast)


@fast_executor.handler
async def propose_fast(ctx, task: Task) -> Fix:
    """A fix of the task's finding, proposed quickly."""
    return await pydantic_ai.prompt(ctx, fast, _asked(task), output_type=Fix)


careful = Agent(
    "openai:gpt-6-sol",
    instructions=EXECUTOR + " Check the versions and the advisory yourself; prefer a change with low risk.",
    tools=TOOLS,
)
careful_executor = pydantic_ai.agent("careful_executor", careful)


@careful_executor.handler
async def propose_carefully(ctx, task: Task) -> Fix:
    """A fix of the task's finding, checked carefully."""
    return await pydantic_ai.prompt(ctx, careful, _asked(task), output_type=Fix)


auditing = Agent(
    "openai:gpt-6-sol",
    instructions=(
        "You audit security fixes before they are made: accept one that fixes the finding (the version is past the "
        "first fixed one), changes nothing else, and can be rolled back; reject anything else, saying why."
    ),
    tools=TOOLS,
)
judge = pydantic_ai.agent("judge", auditing)


@judge.handler
async def audit(ctx, proposal: Proposal) -> Verdict:
    """The judge's verdict on a proposed fix."""
    return await pydantic_ai.prompt(ctx, auditing, f"Audit this fix: {proposal.model_dump_json()}", output_type=Verdict)

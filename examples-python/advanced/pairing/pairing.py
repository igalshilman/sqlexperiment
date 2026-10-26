"""
A pair programmer you talk with about a project, for as long as the project
lasts: each project (the object's key) has a sandbox that keeps its files,
a plan in its database, and a conversation that goes on from one message
to the next. The agent's loop runs here, in Restate: each step of it is
stored, so a crash carries on from the last one, mid-message.

    OPENAI_API_KEY=... uv run advanced/pairing/pairing.py
    restate deployments register http://localhost:9080
    uv run advanced/pairing/pair.py fizzbuzz      # in another terminal: talk with it

Or with curl:

    curl localhost:8080/pair/fizzbuzz/chat/send -d 'Write fizzbuzz.py, with tests, and ship it'
    curl localhost:8080/pair/fizzbuzz/tasks       # the plan, as it works
    curl localhost:8080/pair/fizzbuzz/steer -d 'Use pytest'
    curl localhost:8080/pair/fizzbuzz/signal --json '{"name": "approval", "value": "approved"}'
    curl localhost:8080/pair/fizzbuzz/messages

REVIEWER sets the reviewer's model (GPT-6 Astra by default).
"""

import os

from pydantic import BaseModel, Field
from restate_agents import litellm, serve, signal, subagent, workflow
from shipping import shipping
from tools import PLAN, box, done, edit_file, plan, tasks_of

reviewer = litellm.agent(
    "reviewer",
    model=os.environ.get("REVIEWER", "openai/gpt-6-astra"),
    system=(
        "You review code changes. Read the diff you are given, and answer with what to fix, "
        "most important first: bugs, missing tests, unclear code. If there is nothing to fix, say so."
    ),
)


class Proposal(BaseModel):
    title: str = Field(description="The change, in a line")
    summary: str = Field(description="What changed and why, and what the review said")


approval = signal(
    "approval",
    description="Ask the person to approve a change before you ship it, and wait: approved, or what to change.",
    input=Proposal,
)

pair = litellm.agent(
    "pair",
    system="""You are a pair programmer, working with a person on their project in a sandbox that keeps its files.
- Before you change anything, set a plan with `plan`, and mark each task `done` once it is.
- Read a file before you change it; change it with `edit_file`, and run the tests after each change.
- Keep the project a git repository (`git init` it if it is none), and commit each change that works.
- Once the work is done, hand its diff to the reviewer, and fix what the review finds.
- Ship only what the person approved: ask with `wait_for_approval`, then ship with `shipping`.
Answer briefly: what you did, and what is next.""",
    tools=[
        *box.tools(),
        edit_file,
        plan,
        done,
        subagent(reviewer, description="a reviewer, who reads a diff and says what to fix"),
        approval,
        workflow(shipping, description="Ship an approved change: its checks run, then its pull request opens."),
    ],
    schema=PLAN,
)


@pair.handler(kind="shared")
async def tasks(ctx) -> list[dict]:
    """The plan: shared, so it answers while the pair works."""
    return await tasks_of(ctx.db)


@pair.handler
async def files(ctx) -> list[str]:
    """The files of the project's sandbox."""
    return await box.list_files(ctx)


@pair.handler(message=True)
async def compact(ctx, instructions: str) -> str:
    """Summarize the conversation so far, as ``instructions`` say, in place of it."""
    return await litellm.compact(ctx, instructions)


@pair.handler
async def discard(ctx) -> None:
    """Remove the project's sandbox, and its files."""
    await box.destroy(ctx)


if __name__ == "__main__":
    serve(pair, reviewer, shipping)

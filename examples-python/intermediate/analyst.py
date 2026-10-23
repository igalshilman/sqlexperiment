"""
A data analyst whose code runs in a sandbox: the agent's loop runs here,
and its tools write files and run commands in a Modal Sandbox of the
conversation's own (restate_agents.sandbox), whose files last from one
message to the next.

    ANTHROPIC_API_KEY=... MODAL_TOKEN_ID=... MODAL_TOKEN_SECRET=... uv run intermediate/analyst.py
    restate deployments register http://localhost:9080
    curl localhost:8080/analyst/alice/chat -d 'Simulate 10,000 rolls of two dice: how often is each sum?'
    curl localhost:8080/analyst/alice/chat -d 'Save the counts to counts.csv, sorted by sum'
    curl localhost:8080/analyst/alice/files
    curl localhost:8080/analyst/alice/discard

Without MODAL_TOKEN_ID, the sandbox is a directory of this machine
(``sandbox.local()``): for trying it out, as it isolates nothing.
"""

import os

from restate_agents import claude, sandbox, serve

box = sandbox.modal(image="python:3.13-slim") if os.environ.get("MODAL_TOKEN_ID") else sandbox.local()

analyst = claude.agent(
    "analyst",
    system=(
        "You are a data analyst. Answer with numbers you computed: write Python in your sandbox, run it, "
        "and read its output. Keep your files in the sandbox for later questions."
    ),
    tools=box.tools(),
)


@analyst.handler
async def files(ctx) -> list[str]:
    """The files of the conversation's sandbox."""
    return await box.list_files(ctx)


@analyst.handler
async def discard(ctx) -> None:
    """Remove the conversation's sandbox, and its files."""
    await box.destroy(ctx)


if __name__ == "__main__":
    serve(analyst)

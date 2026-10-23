"""
A coding agent on Claude Code (the Claude Agent SDK): each key has a
workspace of its own, where Claude Code reads, edits and runs code with its
own tools, and a person reviews the change before it is done (a signal).
Its session is kept in the object's SQL database: if the process dies
mid-task, Restate retries and Claude Code carries on from its stored
transcript, on whichever machine runs the retry.

    ANTHROPIC_API_KEY=... uv run coder.py
    restate deployments register http://localhost:9080
    curl localhost:8080/coder/fizzbuzz/chat/send -d 'Write fizzbuzz.py, with a test, and run it'
    curl localhost:8080/coder/fizzbuzz/status                           # what it waits for
    curl localhost:8080/coder/fizzbuzz/signal --json '{"name": "review", "value": "Looks good"}'
    curl localhost:8080/coder/fizzbuzz/messages

Claude Code's tools run on this machine, in /tmp/restate-coder/<key>: run it
in a sandbox (a container, a VM).
"""

from pathlib import Path

from restate_agents import claude_agent, serve, signal


def workspace(ctx) -> Path:
    """The key's workspace."""
    path = Path("/tmp/restate-coder") / ctx.key()
    path.mkdir(parents=True, exist_ok=True)
    return path


review = signal("review", description="Ask a person to review your change, once it works, and wait for their verdict.")

coder = claude_agent.agent(
    "coder",
    system="Once the change works, ask for a review, and do what the reviewer says.",
    tools=[review],
    workspace=workspace,
    claude_options={"max_turns": 50},
)

if __name__ == "__main__":
    serve(coder)

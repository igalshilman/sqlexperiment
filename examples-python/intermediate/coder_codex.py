"""
The coding agent of coder.py, on Codex (OpenAI's coding agent): each key
has a workspace of its own, where Codex reads, edits and runs code with its
own tools, and a person reviews the change before it is done (a signal).
Its thread is kept in the object's SQL database: if the process dies
mid-task, Restate retries and Codex carries on from its stored rollout, on
whichever machine runs the retry.

    OPENAI_API_KEY=... uv run coder_codex.py
    restate deployments register http://localhost:9080
    curl localhost:8080/coder/fizzbuzz/chat/send -d 'Write fizzbuzz.py, with a test, and run it'
    curl localhost:8080/coder/fizzbuzz/status                           # what it waits for
    curl localhost:8080/coder/fizzbuzz/signal --json '{"name": "review", "value": "Looks good"}'
    curl localhost:8080/coder/fizzbuzz/messages

Codex's tools run on this machine, in /tmp/restate-coder/<key>, in its
sandbox (workspace-write): run it in a container or a VM all the same.
"""

from pathlib import Path

from restate_agents import codex, serve, signal


def workspace(ctx) -> Path:
    """The key's workspace."""
    path = Path("/tmp/restate-coder") / ctx.key()
    path.mkdir(parents=True, exist_ok=True)
    return path


review = signal("review", description="Ask a person to review your change, once it works, and wait for their verdict.")

coder = codex.agent(
    "coder",
    system="Once the change works, ask for a review, and do what the reviewer says.",
    tools=[review],
    workspace=workspace,
)

if __name__ == "__main__":
    serve(coder)

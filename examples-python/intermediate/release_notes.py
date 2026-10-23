"""
Release notes by two models, through LiteLLM: a fast, cheap model drafts,
a strong one reviews, and the draft goes back and forth until the reviewer
approves it (at most three rounds). The loop is a handler of your own, and
each model's answer is stored as it comes: a crash in the third round
carries on from there, without asking either model again. The approved
notes go to a publishing workflow, which runs once.

    ANTHROPIC_API_KEY=... uv run release_notes.py
    restate deployments register http://localhost:9080
    curl localhost:8080/releases/v2.4/write --json '["Add Apple Pay", "Fix rounding in the cart"]'

The models are any two LiteLLM knows, with their providers' keys:
DRAFTER=openai/gpt-5-mini REVIEWER=gemini/gemini-2.5-pro, say. By default,
Claude Haiku drafts and Claude Sonnet reviews.
"""

import os

import restate_fns as restate
from restate_agents import Object, litellm, serve

DRAFTER = os.environ.get("DRAFTER", "anthropic/claude-haiku-5-5")
REVIEWER = os.environ.get("REVIEWER", "anthropic/claude-sonnet-5-5")

# A workflow: the notes are published once, however often the release asks.
publishing = restate.Workflow("publishing")


@publishing.main()
async def publish(ctx: restate.WorkflowContext, notes: str) -> str:
    """Publish a release's notes."""
    await ctx.run_typed("post the notes", lambda: print(notes))  # a stand-in for your blog or changelog
    return f"https://example.com/releases/{ctx.key()}"


releases = Object("releases")

drafter = "You write release notes for users: short, plain, grouped by what changed for them."
reviewer = (
    "You review release notes. If they are clear, accurate to the changes and free of jargon, answer APPROVED "
    "and nothing else; otherwise, say what to fix, briefly."
)


@releases.handler
async def write(ctx, changes: list[str]) -> str:
    """Draft, review and publish the notes of the release (the key) of ``changes``."""
    listed = "\n".join(f"- {change}" for change in changes)
    # Each prompt is answered once; each model keeps its own conversation (its thread) of this release.
    draft = await litellm.prompt(ctx, f"The changes:\n{listed}", model=DRAFTER, system=drafter, thread="drafter")
    for _ in range(3):
        review = await litellm.prompt(
            ctx, f"The changes:\n{listed}\n\nThe notes:\n{draft}", model=REVIEWER, system=reviewer, thread="reviewer"
        )
        if review.strip().startswith("APPROVED"):
            break
        draft = await litellm.prompt(
            ctx, f"The reviewer says: {review}", model=DRAFTER, system=drafter, thread="drafter"
        )
    url = await ctx.workflow_call(publish, key=ctx.key(), arg=draft)
    return f"{draft}\n\nPublished at {url}"


if __name__ == "__main__":
    serve(publishing, releases)

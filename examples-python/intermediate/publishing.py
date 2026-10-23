"""
The publishing that writer.py and release_notes.py hand their text to, as a
Restate workflow: a text is published once per workflow (its key), however
often it is asked to be.
"""

import restate_fns as restate

publishing = restate.Workflow("publishing")


@publishing.main()
async def publish(ctx: restate.WorkflowContext, text: str) -> str:
    """Publish a text, once: its URL."""
    await ctx.run_typed("post the text", lambda: print(text))  # a stand-in for your blog or changelog
    return f"https://example.com/posts/{ctx.key()}"

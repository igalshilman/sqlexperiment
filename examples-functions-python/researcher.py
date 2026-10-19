"""A researcher: Claude splits a question into a few angles, studies them all
at once, and writes one answer from the notes.

The fan-out is plain asyncio. While the run waits on Claude, its tasks, the
notes taken so far and the loop over them are saved with the rest of the
interpreter's memory: a crash resumes them, and a model call already
answered is not made again. Each angle has a deadline, as durable as the
calls (asyncio's timers are Restate timers here).

    ANTHROPIC_API_KEY=... target/release/functions
    curl localhost:8080/researcher/alice/research --json \\
      '"Why do tomatoes split after rain?"'
"""

import asyncio

MODEL = "claude-opus-5-5"


async def _ask(ctx, prompt):
    """Claude's answer to `prompt`, as text."""
    response = await ctx.rpc.call(
        "claude",
        "messages",
        {
            "model": MODEL,
            "max_tokens": 16000,
            "output_config": {"effort": "low"},
            "messages": [{"role": "user", "content": prompt}],
        },
    )
    return "".join(block["text"] for block in response["content"] if block["type"] == "text")


async def _study(ctx, question, angle):
    try:
        async with asyncio.timeout(120):
            return await _ask(ctx, f"In a short paragraph, answer from this angle: {angle}\n\n{question}")
    except TimeoutError:
        return f"({angle}: no answer in time)"


async def research(question, ctx):
    plan = await _ask(
        ctx,
        f"List three distinct angles from which to study this question, one per line, and nothing else:\n\n{question}",
    )
    angles = [line.strip("-*• ").strip() for line in plan.splitlines() if line.strip()][:3]
    notes = await asyncio.gather(*(_study(ctx, question, angle) for angle in angles))
    findings = "\n\n".join(f"## {angle}\n{note}" for angle, note in zip(angles, notes, strict=True))
    answer = await _ask(ctx, f"Answer the question from these notes.\n\nQuestion: {question}\n\n{findings}")
    return {"angles": angles, "answer": answer}

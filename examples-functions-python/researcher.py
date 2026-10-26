"""A researcher: a model splits a question into a few angles, studies them all
at once, and writes one answer from the notes.

The fan-out is plain asyncio. While the run waits on the model, its tasks, the
notes taken so far and the loop over them are saved with the rest of the
interpreter's memory: a crash resumes them, and a model call already
answered is not made again. Each angle has a deadline, as durable as the
calls (asyncio's timers are Restate timers here).

    OPENAI_API_KEY=... target/release/functions
    curl localhost:8080/researcher/alice/research --json \\
      '"Why do tomatoes split after rain?"'
"""

import asyncio

MODEL = "openai/gpt-6-sol"


async def _ask(ctx, prompt):
    """The model's answer to `prompt`, as text."""
    response = await ctx.llm.complete(
        {
            "model": MODEL,
            "messages": [{"role": "user", "content": prompt}],
            "options": {"reasoning_effort": "low"},
        }
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

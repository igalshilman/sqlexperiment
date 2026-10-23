"""
Deep research, a run at a time (a workflow, keyed by the run): a planner
splits the question into topics, and waits for you to approve the plan or
say what to change; researchers work the topics at once, each on Wikipedia;
a writer drafts the report, and a critic sends it back until it is good
(three rounds at most).

Every step is Restate's: a crash anywhere carries on from where it was, a
plan can wait for you for days without a process, and each agent's
conversation is in its own object, to read or follow as it works.

    ANTHROPIC_API_KEY=... uv run advanced/research/research.py
    restate deployments register http://localhost:9080
    curl localhost:8080/research/r1/run/send --json '"How did the Dutch close off the Zuiderzee?"'
    curl localhost:8080/research/r1/progress      # the plan, once it waits for you
    curl localhost:8080/research/r1/review --json '{"approved": false, "notes": "Add the 1916 flood"}'
    curl localhost:8080/research/r1/review --json '{"approved": true}'
    uv run basic/follow.py researcher r1.0         # a researcher at work
    curl localhost:8080/research/r1/progress      # once done, the report

Cancel a run (in Restate's UI, or ``restate invocations cancel``) and its
researchers stop with it.
"""

import restate_fns as restate
from pydantic import BaseModel
from restate_agents import serve
from roles import Brief, Plan, critic, draft, investigate, judge, plan, planner, replan, researcher, revise, writer

research = restate.Workflow("research")


class Review(BaseModel):
    """Your review of a plan: approved, or what to change."""

    approved: bool
    notes: str = ""


class Progress(BaseModel):
    stage: str
    plan: Plan | None = None
    report: str | None = None


@research.main()
async def run(ctx: restate.WorkflowContext, question: str) -> str:
    """Research ``question``, and answer with the report."""
    approved = await _approved_plan(ctx, question)
    _progress(ctx, f"researching {len(approved.topics)} topics", approved)
    # The researchers work at once, each in its own object (`researcher/<run>.<n>`).
    calls = [ctx.object_call(investigate, key=f"{ctx.key()}.{n}", arg=t) for n, t in enumerate(approved.topics)]
    await restate.gather(*calls)
    findings = {topic.title: await call for topic, call in zip(approved.topics, calls, strict=True)}
    _progress(ctx, "writing", approved)
    report = await _reviewed_report(ctx, Brief(question=question, findings=findings))
    _progress(ctx, "done", approved, report)
    return report


async def _approved_plan(ctx: restate.WorkflowContext, question: str) -> Plan:
    """The planner's plan, changed until you approve it."""
    proposed = await ctx.object_call(plan, key=ctx.key(), arg=question)
    for n in range(1, 6):
        ctx.set("round", n)
        _progress(ctx, "waiting for your review of the plan", proposed)
        review = await ctx.promise(f"review-{n}", type_hint=Review).value()
        if review.approved:
            return proposed
        proposed = await ctx.object_call(replan, key=ctx.key(), arg=review.notes)
    raise restate.TerminalError("the plan was not approved in five rounds")


async def _reviewed_report(ctx: restate.WorkflowContext, brief: Brief) -> str:
    """The writer's report, revised until the critic approves it (or three rounds went by)."""
    report = await ctx.object_call(draft, key=ctx.key(), arg=brief)
    for _ in range(3):
        verdict = await ctx.object_call(judge, key=ctx.key(), arg=report)
        if verdict.approved:
            break
        report = await ctx.object_call(revise, key=ctx.key(), arg=verdict.notes)
    return report


def _progress(ctx: restate.WorkflowContext, stage: str, plan: Plan, report: str | None = None) -> None:
    ctx.set("progress", Progress(stage=stage, plan=plan, report=report).model_dump())


@research.handler()
async def review(ctx: restate.WorkflowSharedContext, review: Review) -> None:
    """Approve the plan the run waits on, or say what to change."""
    n = await ctx.get("round")
    if n is None:
        raise restate.TerminalError("the run waits for no plan yet")
    await ctx.promise(f"review-{n}", type_hint=Review).resolve(review)


@research.handler()
async def progress(ctx: restate.WorkflowSharedContext) -> Progress:
    """Where the run is: its stage, its plan, and, once done, the report."""
    return Progress(**(await ctx.get("progress") or {"stage": "planning"}))


if __name__ == "__main__":
    serve(research, planner, researcher, writer, critic)

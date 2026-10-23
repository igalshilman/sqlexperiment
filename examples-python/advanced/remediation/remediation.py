"""
A supervisor that remediates security findings, a workflow per finding:
an executor proposes a fix, in a deadline; a judge audits it; a rejected
fix goes up the ladder to a careful executor, told why it was rejected. A
risky or critical fix waits for a person to sign it off (for two days at
most). Then the supervisor opens the ticket and the pull request; if no
fix is accepted, or nobody signs it off, it pages the engineer on call.

Each step is Restate's: every decision is stored once, in order, and kept
as the finding's audit trail; a crash carries on from the last decision; a
sign-off can take days without a process; an executor past its deadline is
cancelled, its work with it.

    ANTHROPIC_API_KEY=... uv run advanced/remediation/remediation.py
    restate deployments register http://localhost:9080
    curl localhost:8080/remediation/f-1/run/send --json @advanced/remediation/finding.json
    curl localhost:8080/remediation/f-1/trail      # each decision, as it is made
    curl localhost:8080/remediation/f-1/sign_off --json '{"approved": true, "by": "ada"}'
    curl localhost:8080/fast_executor/f-1/messages # an executor's conversation

DEADLINE_SECONDS sets each executor's deadline (120 by default).
"""

import os
from collections.abc import Callable
from dataclasses import dataclass
from datetime import timedelta

import restate_fns as restate
from pydantic import BaseModel
from restate_agents import serve
from roles import (
    Finding,
    Fix,
    Proposal,
    Task,
    audit,
    careful_executor,
    fast_executor,
    judge,
    propose_carefully,
    propose_fast,
)
from systems import Change, Ticket, advisories, inventory, open_pull_request, open_ticket, page, pager, repo, tickets


@dataclass(frozen=True)
class Rung:
    """An executor of the ladder: its name, its handler, and its deadline."""

    name: str
    propose: Callable
    deadline: timedelta


DEADLINE = timedelta(seconds=int(os.environ.get("DEADLINE_SECONDS", 120)))
LADDER = [Rung("fast", propose_fast, DEADLINE), Rung("careful", propose_carefully, DEADLINE * 2)]
SIGN_OFF_WITHIN = timedelta(days=2)


class SignOff(BaseModel):
    approved: bool
    by: str
    comment: str = ""


class Decision(BaseModel):
    """An entry of the audit trail: who decided what."""

    by: str
    decision: str
    detail: str


class Trail(BaseModel):
    """The audit trail: every decision, in order."""

    decisions: list[Decision] = []


remediation = restate.Workflow("remediation")


@remediation.main()
async def run(ctx: restate.WorkflowContext, finding: Finding) -> str:
    """Remediate ``finding``: the ticket and pull request opened, or why it was escalated."""
    fix = await _accepted_fix(ctx, finding)
    if fix is None:
        return await _escalate(ctx, finding, "no fix was accepted")
    if _needs_sign_off(finding, fix) and not await _signed_off(ctx):
        return await _escalate(ctx, finding, "the fix was not signed off")
    return await _apply(ctx, finding, fix)


async def _accepted_fix(ctx: restate.WorkflowContext, finding: Finding) -> Fix | None:
    """The first fix of the ladder that the judge accepts; each rejection's reasons go to the next rung."""
    rejected: list[str] = []
    for rung in LADDER:
        fix = await _proposal(ctx, rung, Task(finding=finding, rejected=rejected))
        if fix is None:
            continue
        verdict = await ctx.object_call(audit, key=ctx.key(), arg=Proposal(finding=finding, fix=fix))
        await _record(ctx, "judge", "accepted" if verdict.accepted else "rejected", verdict.reasons)
        if verdict.accepted:
            return fix
        rejected.append(verdict.reasons)
    return None


async def _proposal(ctx: restate.WorkflowContext, rung: Rung, task: Task) -> Fix | None:
    """The fix ``rung``'s executor proposes, or none if it misses its deadline (it is cancelled then)."""
    call = ctx.object_call(rung.propose, key=ctx.key(), arg=task)
    match await restate.select(fix=call, deadline=ctx.sleep(rung.deadline)):
        case ["fix", fix]:
            await _record(ctx, f"{rung.name} executor", "proposed", f"{fix.summary} (risk: {fix.risk})")
            return fix
    await call.cancel_invocation()
    await _record(ctx, f"{rung.name} executor", "timed out", f"no fix in {rung.deadline.total_seconds():.0f} s")
    return None


def _needs_sign_off(finding: Finding, fix: Fix) -> bool:
    """The policy: a person signs off a fix of a critical finding, and a risky fix."""
    return finding.severity == "critical" or fix.risk == "high"


async def _signed_off(ctx: restate.WorkflowContext) -> bool:
    """Whether a person signed the fix off, in time."""
    await _record(ctx, "policy", "waits for a sign-off", f"within {SIGN_OFF_WITHIN.days} days")
    sign_off = ctx.promise("sign_off", type_hint=SignOff).value()
    match await restate.select(sign_off=sign_off, deadline=ctx.sleep(SIGN_OFF_WITHIN)):
        case ["sign_off", given]:
            await _record(ctx, given.by, "signed off" if given.approved else "refused", given.comment)
            return given.approved
    await _record(ctx, "policy", "timed out", "nobody signed off")
    return False


async def _apply(ctx: restate.WorkflowContext, finding: Finding, fix: Fix) -> str:
    title = f"{finding.title}: {fix.summary}"
    body = f"{finding.detail}\n\nFix: {fix.summary}\nRisk: {fix.risk}\nRollback: {fix.rollback}"
    ticket = await ctx.service_call(open_ticket, arg=Ticket(service=finding.service, title=title, body=body))
    pr = await ctx.service_call(open_pull_request, arg=Change(service=finding.service, title=title, diff=fix.diff))
    await _record(ctx, "supervisor", "applied", f"{ticket}, {pr}")
    return f"Opened {ticket} and {pr}"


async def _escalate(ctx: restate.WorkflowContext, finding: Finding, why: str) -> str:
    await ctx.service_call(page, arg=f"{finding.severity} finding of {finding.service}: {finding.title}: {why}")
    await _record(ctx, "supervisor", "escalated", why)
    return f"Escalated: {why}"


async def _record(ctx: restate.WorkflowContext, by: str, decision: str, detail: str) -> None:
    """Add a decision to the audit trail (the workflow's state, which ``trail`` reads)."""
    trail = Trail(**(await ctx.get("trail") or {}))
    trail.decisions.append(Decision(by=by, decision=decision, detail=detail))
    ctx.set("trail", trail.model_dump())


@remediation.handler()
async def sign_off(ctx: restate.WorkflowSharedContext, given: SignOff) -> None:
    """Sign the fix off (or refuse it), as the policy asks."""
    await ctx.promise("sign_off", type_hint=SignOff).resolve(given)


@remediation.handler()
async def trail(ctx: restate.WorkflowSharedContext) -> Trail:
    """The audit trail: every decision, in order."""
    return Trail(**(await ctx.get("trail") or {}))


if __name__ == "__main__":
    serve(remediation, fast_executor, careful_executor, judge, inventory, advisories, tickets, repo, pager)

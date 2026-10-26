"""
The research team, each an agent of its own, keyed by the research run (a
researcher by the run and its topic: ``researcher/r1.2``), with its
conversation in its object's database: a planner and a critic on Pydantic
AI, which answer typed; a researcher and a writer on LiteLLM.

Each role is a handler of the agent's own, which the run calls like any
Restate handler; its conversation goes on from one call to the next, so the
planner revises the plan it made, and the writer the report it wrote.
"""

from pydantic import BaseModel, Field
from pydantic_ai import Agent
from restate_agents import litellm, pydantic_ai
from wiki import read, search

# Pydantic AI's name of the model of the planner and the critic.
MODEL = "openai:gpt-6-sol"


class Topic(BaseModel):
    """A part of the question that one researcher answers."""

    title: str
    questions: list[str] = Field(description="What to find out, as questions a source answers.")


class Plan(BaseModel):
    topics: list[Topic] = Field(min_length=1, max_length=5)


class Brief(BaseModel):
    """What the writer writes from: the question, and each topic's findings."""

    question: str
    findings: dict[str, str]


class Verdict(BaseModel):
    approved: bool
    notes: str = Field(description="What to fix, if not approved: specific, and short.")


planning = Agent(
    MODEL,
    instructions=(
        "You plan research: split a question into two to five topics that do not overlap, each with the questions "
        "that answer it. When a person asks for changes, change the plan as they say."
    ),
)
planner = pydantic_ai.agent("planner", planning)


@planner.handler
async def plan(ctx, question: str) -> Plan:
    """A plan for ``question``."""
    return await pydantic_ai.prompt(ctx, planning, f"Plan the research of: {question}", output_type=Plan)


@planner.handler
async def replan(ctx, notes: str) -> Plan:
    """The plan, changed as a person's ``notes`` say."""
    return await pydantic_ai.prompt(ctx, planning, f"Change the plan: {notes}", output_type=Plan)


RESEARCHER = (
    "You research one topic on Wikipedia: search, read the articles that matter, and answer each question with the "
    "facts you found, citing the articles by title. Say what you could not find."
)
researcher = litellm.agent("researcher", system=RESEARCHER, tools=[search, read])


@researcher.handler
async def investigate(ctx, topic: Topic) -> str:
    """The findings of ``topic``."""
    questions = "\n".join(f"- {q}" for q in topic.questions)
    return await litellm.prompt(ctx, f"Topic: {topic.title}\n{questions}", system=RESEARCHER, tools=[search, read])


WRITER = "You write research reports: clear, structured, every claim from the findings, with the sources at the end."
writer = litellm.agent("writer", system=WRITER)


@writer.handler
async def draft(ctx, brief: Brief) -> str:
    """A report answering the brief's question from its findings."""
    findings = "\n\n".join(f"## {title}\n{found}" for title, found in brief.findings.items())
    return await litellm.prompt(ctx, f"Question: {brief.question}\n\nFindings:\n\n{findings}", system=WRITER)


@writer.handler
async def revise(ctx, notes: str) -> str:
    """The report, revised as the critic's ``notes`` say."""
    return await litellm.prompt(ctx, f"Revise the report: {notes}\nAnswer with the whole report.", system=WRITER)


reviewing = Agent(
    MODEL,
    instructions=(
        "You review research reports for a careful reader: approve one that answers the question, claims nothing "
        "the findings do not support and cites its sources; otherwise, say what to fix."
    ),
)
critic = pydantic_ai.agent("critic", reviewing)


@critic.handler
async def judge(ctx, report: str) -> Verdict:
    """The critic's verdict on ``report``: in the same conversation, so it checks its notes were followed."""
    return await pydantic_ai.prompt(ctx, reviewing, f"Review this report:\n\n{report}", output_type=Verdict)

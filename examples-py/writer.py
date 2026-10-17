"""
A writer and an editor, on a LangGraph graph of your own: the writer
drafts, the editor reviews, and the writer revises until the editor
approves, three drafts at most. Each draft and review is a step stored in
the object's database: a crash mid-review carries on from the last draft,
without writing it again.

    ANTHROPIC_API_KEY=... uv run writer.py
    restate deployments register http://localhost:9080
    curl localhost:8080/writer/launch/chat -d 'A tweet announcing durable AI agents on Restate'
    curl localhost:8080/writer/launch/messages   # each draft
"""

from langchain_anthropic import ChatAnthropic
from langchain_core.messages import HumanMessage, SystemMessage
from langgraph.graph import END, START, MessagesState, StateGraph
from pydantic import BaseModel
from restate_agents import langgraph, serve

model = ChatAnthropic(model="claude-sonnet-5-5")

DRAFTS = 3


class Draft(MessagesState):
    notes: str  # the editor's notes on the last draft, if it was not approved
    drafts: int  # the drafts of this request so far


class Review(BaseModel):
    approved: bool
    notes: str


async def write(state: Draft) -> dict:
    """A draft, or a revision of the last one, with the editor's notes."""
    first = isinstance(state["messages"][-1], HumanMessage)
    task = "Write it." if first else f"Revise your last draft, as the editor says: {state['notes']}"
    system = SystemMessage(f"You are a writer. {task} Answer with the text only.")
    draft = await model.ainvoke([system, *state["messages"]])
    return {"messages": [draft], "drafts": 1 if first else state["drafts"] + 1}


async def review(state: Draft) -> dict:
    """The editor's review of the last draft: approved, or notes on what to change."""
    request = next(m.text for m in reversed(state["messages"]) if isinstance(m, HumanMessage))
    draft = state["messages"][-1].text
    system = SystemMessage("You are an editor: approve the draft, or say briefly what to change.")
    asked = HumanMessage(f"The request: {request}\n\nThe draft:\n{draft}")
    verdict = await model.with_structured_output(Review).ainvoke([system, asked])
    return {"notes": "" if verdict.approved else verdict.notes}


def revise_or_end(state: Draft) -> str:
    return "write" if state["notes"] and state["drafts"] < DRAFTS else END


graph = StateGraph(Draft)
graph.add_node("write", write)
graph.add_node("review", review)
graph.add_edge(START, "write")
graph.add_edge("write", "review")
graph.add_conditional_edges("review", revise_or_end)

writer = langgraph.agent("writer", graph=graph)

if __name__ == "__main__":
    serve(writer)

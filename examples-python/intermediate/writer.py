"""
A writer and two editors, on a LangGraph graph of your own: the writer
drafts, the model editor reviews, and the writer revises until it approves
(three drafts at most); then a person approves the draft (a Restate
signal), or sends notes back to the writer; and the publishing workflow
publishes it. Each step is stored in the object's database: a crash
mid-review carries on from the last draft, and the post is published once.

    ANTHROPIC_API_KEY=... uv run intermediate/writer.py
    restate deployments register http://localhost:9080
    curl localhost:8080/writer/post/chat -d 'A tweet announcing durable AI agents on Restate'
    curl localhost:8080/writer/post/pending    # the draft that waits for your approval
    curl localhost:8080/writer/post/signal --json '{"name": "approval", "value": {"approved": false, "notes": "Short"}}'
    curl localhost:8080/writer/post/signal --json '{"name": "approval", "value": {"approved": true}}'
    curl localhost:8080/writer/post/messages   # each draft, and where it was published
"""

import restate_fns as restate
from langchain_anthropic import ChatAnthropic
from langchain_core.messages import AIMessage, HumanMessage, SystemMessage
from langgraph.graph import END, START, MessagesState, StateGraph
from pydantic import BaseModel
from restate_agents import langgraph, serve
from restate_agents.langgraph import nodes

model = ChatAnthropic(model="claude-sonnet-5-5")

DRAFTS = 3


class Draft(MessagesState):
    notes: str  # an editor's notes on the last draft, if it was not approved
    drafts: int  # the drafts of this request so far
    approved: bool  # whether the person approved the last draft


class Review(BaseModel):
    approved: bool
    notes: str = ""


publishing = restate.Workflow("publishing")


@publishing.main()
async def publish(ctx: restate.WorkflowContext, post: str) -> str:
    """Publish a post, once per workflow (a stand-in for the blog's API): its URL."""
    return f"https://blog.example.com/posts/{ctx.key()}"


async def write(state: Draft) -> dict:
    """A draft, or a revision of the last one, with the editor's notes."""
    first = isinstance(state["messages"][-1], HumanMessage)
    task = "Write it." if first else f"Revise your last draft, as the editor says: {state['notes']}"
    system = SystemMessage(f"You are a writer. {task} Answer with the text only.")
    draft = await model.ainvoke([system, *state["messages"]])
    return {"messages": [draft], "drafts": 1 if first else state["drafts"] + 1}


async def review(state: Draft) -> dict:
    """The model editor's review of the last draft: approved, or notes on what to change."""
    request = next(m.text for m in reversed(state["messages"]) if isinstance(m, HumanMessage))
    draft = state["messages"][-1].text
    system = SystemMessage("You are an editor: approve the draft, or say briefly what to change.")
    asked = HumanMessage(f"The request: {request}\n\nThe draft:\n{draft}")
    verdict = await model.with_structured_output(Review).ainvoke([system, asked])
    return {"notes": "" if verdict.approved else verdict.notes}


def revise_or_ask(state: Draft) -> str:
    return "write" if state["notes"] and state["drafts"] < DRAFTS else "approval"


def judged(state: Draft, value: dict) -> dict:
    """The person's review, from the approval signal's value."""
    verdict = Review.model_validate(value)
    return {"approved": verdict.approved, "notes": verdict.notes}


graph = StateGraph(Draft)
graph.add_node("write", write)
graph.add_node("review", review)
# Restate's steps: wait for a person's approval of the last draft; publish it.
graph.add_node(
    "approval", nodes.signal("approval", input=lambda state: {"draft": state["messages"][-1].text}, update=judged)
)
graph.add_node(
    "publish",
    nodes.workflow(
        publishing,
        input=lambda state: state["messages"][-1].text,
        update=lambda state, url: {"messages": [AIMessage(f"Published: {url}")]},
    ),
)
graph.add_edge(START, "write")
graph.add_edge("write", "review")
graph.add_conditional_edges("review", revise_or_ask)
graph.add_conditional_edges("approval", lambda state: "publish" if state["approved"] else "write")
graph.add_edge("publish", END)

writer = langgraph.agent("writer", graph=graph)

if __name__ == "__main__":
    serve(writer, publishing)

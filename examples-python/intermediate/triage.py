"""
A support desk on a LangGraph graph of your own: a router reads each
message and hands it to the billing or the tech team, each with its own
instructions and tools. The graph's state (the conversation, and the team
that answers it) is stored in the object's database at each step: a crash
mid-turn carries on from the step it was at.

    ANTHROPIC_API_KEY=... uv run intermediate/triage.py
    restate deployments register http://localhost:9080
    curl localhost:8080/desk/alice/chat -d 'I was charged twice for order 103, $30 each time'
    curl localhost:8080/desk/alice/chat -d 'And the app crashes when I log in'
    curl localhost:8080/desk/alice/messages
"""

from collections.abc import Callable
from typing import Literal

from langchain_anthropic import ChatAnthropic
from langchain_core.messages import SystemMessage
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.prebuilt import tools_condition
from pydantic import BaseModel
from restate_agents import langgraph, serve, tool

model = ChatAnthropic(model="claude-sonnet-5-5")


@tool
def refund(order: str, amount: float) -> str:
    """Refund an amount of an order."""
    return f"Refunded ${amount} for order {order}."  # a stand-in for the payments API


@tool
def known_issues(product: str) -> str:
    """The known issues of a product, and their workarounds."""
    return "Logging in crashes version 2.3 of the app: version 2.4 fixes it."  # a stand-in for the issue tracker


TEAMS = {
    "billing": ("You are the billing team: charges, refunds and invoices. Be brief.", [refund]),
    "tech": ("You are tech support: the app, accounts and bugs. Be brief.", [known_issues]),
}


class Desk(MessagesState):
    team: str  # the team that answers the last message


class Route(BaseModel):
    team: Literal["billing", "tech"]


async def route(state: Desk) -> dict:
    """Choose the team that answers the customer's last message."""
    system = SystemMessage("Which team answers the customer's last message: billing, or tech?")
    chosen = await model.with_structured_output(Route).ainvoke([system, *state["messages"]])
    return {"team": chosen.team}


def team(system: str, tools: list) -> Callable:
    """A team's node: its model, with its instructions and tools."""

    async def answer(state: Desk) -> dict:
        with_tools = model.bind_tools(langgraph.tools(tools))
        return {"messages": [await with_tools.ainvoke([SystemMessage(system), *state["messages"]])]}

    return answer


graph = StateGraph(Desk)
graph.add_node("route", route)
graph.add_edge(START, "route")
graph.add_conditional_edges("route", lambda state: state["team"], list(TEAMS))
for name, (system, tools) in TEAMS.items():
    graph.add_node(name, team(system, tools))
    graph.add_node(f"{name}_tools", langgraph.tool_node(tools))
    graph.add_conditional_edges(name, tools_condition, {"tools": f"{name}_tools", END: END})
    graph.add_edge(f"{name}_tools", name)

desk = langgraph.agent("desk", graph=graph)

if __name__ == "__main__":
    serve(desk)

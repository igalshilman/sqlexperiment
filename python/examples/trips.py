"""
A travel agent on Pydantic AI: an Agent of your own, with a Pydantic AI tool
function and a Restate tool that waits for the traveller to approve a
booking; and a handler of your own that asks it for the plan, typed. The
conversation is stored in the object's database at each step: a crash
mid-turn carries on from the step it was at.

    ANTHROPIC_API_KEY=... uv run trips.py
    restate deployments register http://localhost:9080
    curl localhost:8080/trips/alice/chat -d 'Three days in Lisbon in May, and book me a hotel'
    curl localhost:8080/trips/alice/pending      # the booking it waits to be approved
    curl localhost:8080/trips/alice/signal --json '{"name": "approval", "value": "Yes, book it"}'
    curl localhost:8080/trips/alice/plan         # the trip, day by day, as JSON
"""

from pydantic import BaseModel
from pydantic_ai import Agent
from restate_agents import serve, signal, tool
from restate_agents.pydantic_ai import agent, prompt, toolset


class Booking(BaseModel):
    hotel: str
    nights: int


class Day(BaseModel):
    morning: str
    afternoon: str
    evening: str


class Plan(BaseModel):
    city: str
    days: list[Day]


def weather(city: str, month: str) -> str:
    """The usual weather of a city in a month."""
    return f"{city} in {month}: mostly sunny, 22°C."  # a stand-in for a weather service


@tool
def book(call_id: str, hotel: str, nights: int) -> str:
    """Book a hotel, once the traveller approved it."""
    return f"Booked {nights} nights at {hotel}: booking {call_id[:8]}."  # a stand-in, idempotent by its call id


planner = Agent(
    "anthropic:claude-sonnet-5-5",
    instructions="You are a travel agent: plan trips, and book what the traveller approves. Be brief.",
    tools=[weather],
    toolsets=[
        toolset([signal("approval", description="Ask the traveller to approve a booking.", input=Booking), book])
    ],
)

trips = agent("trips", planner)


@trips.handler
async def plan(ctx) -> Plan:
    """The trip so far, day by day."""
    return await prompt(ctx, planner, "Write down the trip we planned, day by day.", output_type=Plan)


if __name__ == "__main__":
    serve(trips)

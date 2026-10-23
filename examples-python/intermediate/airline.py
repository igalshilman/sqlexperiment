"""
An airline's customer service on the OpenAI Agents SDK, its steps Restate's:
a triage agent hands each traveller off to the flight changes agent, which
looks flights up with a Restate service, asks the traveller to confirm the
change fee (a signal, which may take hours), and rebooks with a workflow
that charges the fee once (both in flights.py). The new booking goes into the traveller's own SQL
table, which a handler of your own serves.

A handoff holds: the traveller's next message goes to the changes agent. A
crash anywhere (mid-handoff, mid-wait, mid-rebooking) changes nothing:
Restate retries, and the conversation carries on from its last stored step.

    OPENAI_API_KEY=... uv run intermediate/airline.py
    restate deployments register http://localhost:9080
    curl localhost:8080/airline/alice/chat/send -d 'My flight RA 102 to Lisbon is delayed, can I take an earlier one?'

The changes agent waits for the traveller to confirm the fee; see what it
waits for, and confirm it (as the traveller would, from a link on their phone):

    curl localhost:8080/airline/alice/pending
    curl localhost:8080/airline/alice/signal --json '{"name": "fee", "value": "yes, go ahead"}'
    curl localhost:8080/airline/alice/messages        # the handoff, the tool calls, the answer
    curl localhost:8080/airline/alice/bookings        # the new booking, from the traveller's table
"""

from agents import Agent
from flights import Change, flights, options, rebooking
from restate_agents import call, openai_agents, serve, signal, tool, workflow


@tool
async def record_booking(ctx, call_id, flight: str, ticket: str) -> str:
    """Record the traveller's new booking, once it is rebooked."""
    # Keyed by the tool call's id: a retried call records it once.
    await ctx.db.execute("INSERT OR IGNORE INTO bookings VALUES (?, ?, ?)", [call_id, flight, ticket])
    return f"Recorded {flight}, ticket {ticket}"


changes = Agent(
    name="changes",
    instructions=(
        "You change travellers' flights. Look up the options, offer them, and once the traveller chose, "
        "ask them to confirm the fee (even if it is none), rebook, and record the booking."
    ),
    tools=openai_agents.tools(
        [
            call(options),
            signal("fee", description="Ask the traveller to confirm the change, and its fee, and wait.", input=Change),
            workflow(rebooking, description="Rebook the traveller, once they confirmed the fee."),
            record_booking,
        ]
    ),
)

triage = Agent(
    name="triage",
    instructions="You are an airline's first line. Hand travellers who want to change a flight to the changes agent.",
    handoffs=[changes],
)

# The traveller's bookings: the schema of each traveller's database, beside the conversation.
airline = openai_agents.agent(
    "airline", triage, schema=["CREATE TABLE bookings (id TEXT PRIMARY KEY, flight TEXT, ticket TEXT)"]
)


@airline.handler(kind="shared")
async def bookings(ctx) -> list[dict[str, str]]:
    """The traveller's bookings, from their own table."""
    return [{"flight": f, "ticket": t} for f, t in await ctx.db.query("SELECT flight, ticket FROM bookings")]


if __name__ == "__main__":
    serve(flights, rebooking, airline)

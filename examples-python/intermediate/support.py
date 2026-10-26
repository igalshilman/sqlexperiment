"""
A support agent whose tools are Restate itself: it looks orders up with a
plain service, waits for a manager's approval (a signal), and refunds with
a workflow (both in shop.py). Every step is durable: a crash, or a manager who takes a day,
changes nothing.

    OPENAI_API_KEY=... uv run intermediate/support.py
    restate deployments register http://localhost:9080
    curl localhost:8080/support/alice/chat -d 'Order 103 arrived broken, can I get my $30 back?'

The agent waits for approval; in another terminal, see what it waits for
(the refund, with the wait's id), and approve it:

    curl localhost:8080/support/alice/pending
    curl localhost:8080/support/alice/signal --json '{"id": "<the id>", "value": "approved"}'

or the oldest wait for an approval: ``{"name": "approval", "value": "approved"}``.
While it works, add to the message it answers (it reads it after its next
tool call), see what it does, or stop it:

    curl localhost:8080/support/alice/steer -d 'Actually, make it $20'
    curl localhost:8080/support/alice/status
    curl localhost:8080/support/alice/stop
"""

from restate_agents import call, langgraph, serve, signal, workflow
from shop import Refund, orders, refund, status

support = langgraph.agent(
    "support",
    tools=[
        call(status),
        signal(
            "approval",
            description="Wait for a manager to decide on a refund. Answers with the manager's decision.",
            input=Refund,
        ),
        workflow(refund),
    ],
)

if __name__ == "__main__":
    serve(orders, refund, support)

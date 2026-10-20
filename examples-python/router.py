"""
A front desk that routes each message to the team that handles it: a
handler of your own, where your code decides, and the agents answer.

The desk asks a model which team it is (``claude.prompt``, answered once),
then sends the message to that team's agent (``ask``, sent once). Each team
keeps its own conversation with each user (``billing/alice``, ``tech/alice``).

    ANTHROPIC_API_KEY=... uv run router.py
    restate deployments register http://localhost:9080
    curl localhost:8080/desk/alice/chat -d 'I was charged twice for order 103'
    curl localhost:8080/desk/alice/chat -d 'The app crashes when I open my orders'
    curl localhost:8080/billing/alice/messages   # what billing was asked, and answered
"""

from restate_agents import Object, ask, claude, langgraph, serve

billing = claude.agent(
    "billing",
    system="You handle billing: charges, refunds and invoices. Be brief and kind.",
)

tech = langgraph.agent(
    "tech",
    system="You handle technical problems with the app. Ask for what you need to know, one question at a time.",
)

desk = Object("desk")


@desk.chat
async def chat(ctx, message: str) -> str:
    # Asked once: a retried attempt gets the same answer.
    team = await claude.prompt(
        ctx,
        f"Which team handles this message, billing or tech? Answer with the one word.\n\n{message}",
        request={"output_config": {"effort": "low"}},
    )
    return await ask(ctx, billing if "billing" in team.lower() else tech, message)


if __name__ == "__main__":
    serve(desk, billing, tech)

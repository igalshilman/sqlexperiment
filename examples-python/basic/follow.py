"""
Follow an agent's conversation from a terminal, as it changes: its messages,
its tool calls as it makes them, and what it waits for. It needs nothing of
the agent's code, only Restate's address.

    uv run intermediate/support.py   # an agent to follow, in another terminal
    uv run basic/follow.py support alice 'Order 103 arrived broken, can I get my $30 back?'
    curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'

A chat page for the same conversation is in examples-typescript/ui.
"""

import asyncio
import sys

from restate_agents.client import AgentClient, as_text


async def follow(agent: str, key: str, message: str | None = None) -> None:
    async with AgentClient("http://localhost:8080", agent, key) as conversation:
        if message:
            await conversation.send(message)
        async for change in conversation.follow():
            for said in change.messages:
                print(as_text(said))
            for wait in change.waiting:
                print(f"  waiting for {wait['name']} (id {wait['id']})")


if __name__ == "__main__":
    asyncio.run(follow(*sys.argv[1:4]))

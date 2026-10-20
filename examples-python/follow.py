"""
Follow an agent's conversation from a terminal, as it changes: its messages,
the tool calls as the agent makes them, and what it waits for. Optionally,
send it a message first. It needs nothing of the agent's code, only
Restate's address (``restate_agents.client``).

    uv run support.py   # an agent to follow, in another terminal
    uv run follow.py support alice 'Order 103 arrived broken, can I get my $30 back?'
    curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'

RESTATE is Restate's ingress, http://localhost:8080 by default. A chat page
for the same conversation is in examples-typescript/ui.
"""

import asyncio
import json
import os
import sys

from restate_agents.client import AgentClient


def show(message: dict) -> None:
    for part in message["parts"]:
        if part["type"] == "text":
            print(f"{message['role']}: {part['text']}")
        else:
            output = part.get("output", "…")
            print(f"  {part['name']}({json.dumps(part['input'])}) -> {output}")


async def main(agent: str, key: str, message: str | None) -> None:
    async with AgentClient(os.environ.get("RESTATE", "http://localhost:8080"), agent, key) as conversation:
        if message:
            await conversation.send(message)
        shown: dict[int, dict] = {}
        async for change in conversation.follow():
            # A message may come again, changed (a tool call with its output): show it again.
            for m in change.messages:
                if shown.get(m["id"]) != m:
                    shown[m["id"]] = m
                    show(m)
            for wait in change.status["waiting"]:
                print(f"  waiting for {wait['name']}: {json.dumps(wait['input'])} (id {wait['id']})")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit("follow.py <agent> <key> [message]")
    asyncio.run(main(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None))

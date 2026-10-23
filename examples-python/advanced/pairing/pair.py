"""
Talk with the pair from a terminal, about a project (its key):

    uv run advanced/pairing/pair.py fizzbuzz

What you type goes to the pair; while it works on a message, a line steers
it, at its next tool call. Its messages and its tool calls show as it makes
them, and so do its waits for your approval. Commands:

    /approve [what to change]   answer its wait for your approval
    /tasks                      the plan
    /stop                       stop the message it works on
    /compact                    summarize the conversation so far, in place of it

Ctrl-D leaves. The pair carries on without you, and running this again shows
the conversation so far, and follows it from there.
"""

import asyncio
import json
import sys

from restate_agents.client import AgentClient, AgentError, Followed

INGRESS = "http://localhost:8080"


class Terminal:
    """What the terminal showed of the conversation: each part of a message once, and a tool's output once."""

    def __init__(self) -> None:
        self.shown: set[tuple] = set()
        # The first change is the conversation so far, your messages in it too; after it, you typed them.
        self.history = True

    def show(self, change: Followed) -> None:
        for message in change.messages:
            if message["role"] == "user" and not self.history:
                continue
            for i, part in enumerate(message["parts"]):
                self.part((message["id"], i), message["role"], part)
        for wait in change.waiting:
            self.once(
                (wait["id"],),
                f"⏸  {wait['name']}: {json.dumps(wait['input'])}  (/approve, or /approve <what to change>)",
            )
        self.history = False

    def part(self, key: tuple, role: str, part: dict) -> None:
        if part["type"] == "text":
            self.once(key, f"{'you' if role == 'user' else 'pair'}: {part['text']}")
            return
        self.once(key, f"  → {part['name']}({clipped(json.dumps(part['input']))})")
        if "output" in part:
            self.once((*key, "output"), f"  ← {clipped(str(part['output']))}")

    def once(self, key: tuple, text: str) -> None:
        if key not in self.shown:
            self.shown.add(key)
            print(text, flush=True)


def clipped(text: str, limit: int = 160) -> str:
    """``text`` on one line, at most ``limit`` characters of it."""
    line = text.replace("\n", " ⏎ ")
    return line if len(line) <= limit else line[: limit - 1] + "…"


async def say(conversation: AgentClient, line: str) -> None:
    """A command, or a message: to the message the pair works on, if any, or else as one of its own."""
    command, _, rest = line.partition(" ")
    match command:
        case "/approve":
            if await conversation.signal(rest or "approved", name="approval") is None:
                print("Nothing waits for your approval.")
        case "/tasks":
            for task in await conversation.call("tasks"):
                print(f"  [{'x' if task['done'] else ' '}] {task['n']}. {task['task']}")
        case "/stop":
            await conversation.stop()
        case "/compact":
            instructions = rest or "Keep what we decided, and what is left to do."
            print(await conversation.call("compact", instructions, timeout=None))
        case _:
            await conversation.steer(line)


async def pair(project: str) -> None:
    terminal = Terminal()
    async with AgentClient(INGRESS, "pair", project) as conversation:

        async def follow() -> None:
            async for change in conversation.follow():
                terminal.show(change)

        following = asyncio.create_task(follow())
        while line := await asyncio.to_thread(sys.stdin.readline):
            try:
                if line.strip():
                    await say(conversation, line.strip())
            except AgentError as error:
                print(f"Restate answered {error}")
        following.cancel()


if __name__ == "__main__":
    asyncio.run(pair(sys.argv[1]))

"""
A conversation in the object's database, for the integrations that run the
model's loop themselves (Claude, LiteLLM): the API's own messages, a row
each, in order, each with the prompt it belongs to, and the answer on the
message that answers it.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

import restate_fns as restate

from restate_agents import turns
from restate_agents.common import table_schema

# The model's acknowledgment of a summary or a handoff, which the conversation then carries on from.
CARRY_ON = "Understood: I will carry on from there."


@dataclass
class Row:
    """A message of the conversation: its prompt, the answer if it answers it, and the messages steered into it."""

    prompt: str
    message: dict[str, Any]
    answer: str | None = None
    steered: list[str] = field(default_factory=list)


class Conversation:
    """The conversation ``thread``, in the table ``table`` of the object's database."""

    def __init__(self, db: Any, table: str, thread: str) -> None:
        self.db = db
        self.table = table
        self.thread = thread
        self.length = 0

    async def load(self) -> list[Row]:
        await table_schema(
            self.table,
            f"""
            CREATE TABLE {self.table} (
              thread TEXT NOT NULL,
              n INTEGER NOT NULL,
              prompt TEXT NOT NULL,
              message TEXT NOT NULL,
              answer TEXT,
              steered TEXT,
              PRIMARY KEY (thread, n)
            )
            """,
        ).ensure(self.db)
        rows = await self.db.query(
            f"SELECT prompt, message, answer, steered FROM {self.table} WHERE thread = ? ORDER BY n", [self.thread]
        )
        self.length = len(rows)
        return [
            Row(prompt, json.loads(message), answer, json.loads(steered or "[]"))
            for prompt, message, answer, steered in rows
        ]

    async def append(
        self, prompt: str, *messages: dict[str, Any], answer: str | None = None, steered: Sequence[str] = ()
    ) -> None:
        """Store ``messages`` of ``prompt`` in one transaction: the last with its ``answer``, and ``steered``."""
        async with self.db.transaction() as tx:
            for i, message in enumerate(messages):
                last = i == len(messages) - 1
                await tx.execute(
                    f"INSERT INTO {self.table} VALUES (?, ?, ?, ?, ?, ?)",
                    [
                        self.thread,
                        self.length,
                        prompt,
                        json.dumps(message),
                        answer if last else None,
                        json.dumps(list(steered)) if last and steered else None,
                    ],
                )
                self.length += 1
        await turns.publish()

    async def replace(self, prompt: str, note: str, *, answer: str) -> None:
        """
        The conversation, in place of all it was, in one transaction: ``note``
        and its acknowledgment, as ``prompt``'s, answered ``answer``; or
        nothing, without a note.
        """
        messages = [{"role": "user", "content": note}, {"role": "assistant", "content": CARRY_ON}] if note else []
        async with self.db.transaction() as tx:
            await tx.execute(f"DELETE FROM {self.table} WHERE thread = ?", [self.thread])
            for n, message in enumerate(messages):
                await tx.execute(
                    f"INSERT INTO {self.table} VALUES (?, ?, ?, ?, ?, NULL)",
                    [self.thread, n, prompt, json.dumps(message), answer if n == len(messages) - 1 else None],
                )
        self.length = len(messages)
        await turns.publish()


def answer_of(rows: list[Row], prompt: str) -> str | None:
    """The answer to ``prompt``, if it was answered."""
    return next((row.answer for row in rows if row.prompt == prompt and row.answer is not None), None)


def pending_of(rows: list[Row]) -> str | None:
    """The prompt that a crash cut off, if the last message does not answer its prompt."""
    return rows[-1].prompt if rows and rows[-1].answer is None else None


def idle(rows: list[Row]) -> None:
    """Refuse to change a conversation whose last message is still being answered."""
    if pending_of(rows) is not None:
        raise restate.TerminalError("a message is being answered: change the conversation once it is")


async def reset(conversation: Conversation, ctx: Any, handoff: str | None) -> None:
    """Start ``conversation`` over, with ``handoff`` as what to carry on with, if given."""
    idle(await conversation.load())
    note = f"Before this conversation: {handoff}" if handoff else ""
    await conversation.replace(turns.prompt_id(ctx), note, answer=handoff or "")


# What compacting asks the model, before its instructions.
SUMMARIZE = "Summarize our conversation so far, in place of it: what was asked and answered, and what to carry on with."

# The note a summary replaces the conversation with.
SUMMARIZED = "Our conversation so far, summarized:\n\n"

"""
What the integrations whose framework runs the model's loop and keeps its
own transcript (Claude Code, Codex, the OpenAI Agents SDK, Google ADK) share,
around that transcript, which each stores in the object's database:

- each message and its answer (``Prompts``, ``answer_once``): a message is
  answered once per invocation, and one that a crash cut off is finished
  before the next (for a harness, carried on: ``CARRY_ON``);
- each tool call of the tools here, with its input and output (``Calls``):
  a call that the model makes again with the same input, in a later attempt
  of the same message, is the earlier call (its id, its output);
- the messages steered to the turn (``steer``).
"""

from __future__ import annotations

import json
import tempfile
import uuid
from collections.abc import Awaitable, Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import restate_fns as restate

from restate_agents import turns
from restate_agents.common import has_table
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

# What a retried attempt asks the harness, to finish a message that a crash cut off.
CARRY_ON = (
    "Your work on my last message was cut off (the process stopped), so it may have no answer yet. "
    "Carry on from where you stopped, and answer it. The tool calls that finished have their results: "
    "do not make them again. My message was:\n\n"
)

# How a message steered to the turn reaches the model.
STEERED = "The user added, while you worked: "


@dataclass
class Prompt:
    """A message of the conversation: its prompt's id, its text, its answer once there is one, and what was steered."""

    prompt: str
    text: str
    answer: str | None = None
    steered: list[str] = field(default_factory=list)


class Prompts:
    """The messages of the conversation ``thread``, in order, in the table ``table`` of the object's database."""

    def __init__(self, db: Any, table: str, thread: str) -> None:
        self.db = db
        self.table = table
        self.thread = thread

    async def load(self) -> list[Prompt]:
        if not await has_table(self.db, self.table):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {self.table} (
                  thread TEXT NOT NULL,
                  prompt TEXT NOT NULL,
                  text TEXT NOT NULL,
                  answer TEXT,
                  steered TEXT NOT NULL DEFAULT '[]',
                  PRIMARY KEY (thread, prompt)
                )
            """)
        rows = await self.db.query(
            f"SELECT prompt, text, answer, steered FROM {self.table} WHERE thread = ? ORDER BY rowid", [self.thread]
        )
        return [Prompt(prompt, text, answer, json.loads(steered)) for prompt, text, answer, steered in rows]

    async def add(self, prompt: str, text: str) -> Prompt:
        await self.db.execute(
            f"INSERT INTO {self.table} (thread, prompt, text) VALUES (?, ?, ?)", [self.thread, prompt, text]
        )
        return Prompt(prompt, text)

    async def answer(self, prompt: str, answer: str) -> None:
        await self.db.execute(
            f"UPDATE {self.table} SET answer = ? WHERE thread = ? AND prompt = ?", [answer, self.thread, prompt]
        )
        await turns.publish()

    async def steer(self, prompt: str, ids: Sequence[str]) -> None:
        await self.db.execute(
            f"UPDATE {self.table} SET steered = (SELECT json_group_array(value) FROM "
            f"(SELECT value FROM json_each(steered) UNION ALL SELECT value FROM json_each(?))) "
            "WHERE thread = ? AND prompt = ?",
            [json.dumps(list(ids)), self.thread, prompt],
        )

    async def idle(self) -> None:
        """Refuse to change a conversation whose last message is still being answered."""
        if any(p.answer is None for p in await self.load()):
            raise restate.TerminalError("a message is being answered: change the conversation once it is")


async def answer_once(
    ctx: Any,
    prompts: Prompts,
    text: str,
    run: Callable[[Prompt, str], Awaitable[str]],
    *,
    cancelled: Callable[[Prompt], Awaitable[None]] | None = None,
    steered: Callable[[], Awaitable[list[str]]] | None = None,
) -> str:
    """
    Answer ``text``, once per invocation, with ``run(prompt, message)``: the
    message is the text, or, for a prompt that a crash cut off (this one's,
    or an earlier one's, which is finished first), ``CARRY_ON`` and its text.

    ``cancelled``: what ends a cancelled prompt's transcript, before its
    answer is ``CANCELLED``. ``steered``: the ids of the messages steered to
    the turn that the transcript has, where a framework keeps them in it; by
    default, those ``steer`` recorded.
    """
    id = turns.prompt_id(ctx)
    while True:
        rows = await prompts.load()
        if (asked := next((r for r in rows if r.prompt == id), None)) is not None:
            turns.taken(await steered() if steered is not None else asked.steered)
            if asked.answer is not None:
                return asked.answer
        pending = next((r for r in rows if r.answer is None), None)
        if pending is None:
            pending, message = await prompts.add(id, text), text
        else:
            message = CARRY_ON + pending.text
        try:
            answer = await run(pending, message)
        except restate.TerminalError as error:
            if error.status_code == 409:
                if cancelled is not None:
                    await cancelled(pending)
                await prompts.answer(pending.prompt, CANCELLED)
            raise
        await prompts.answer(pending.prompt, answer)


async def steer(ctx: Any, prompts: Prompts, prompt: Prompt) -> list[str]:
    """The messages steered to the turn since its last tool call, which the conversation of ``prompt`` now has."""
    steered = await turns.steering(ctx)
    if steered:
        await prompts.steer(prompt.prompt, [s.id for s in steered])
        turns.taken(s.id for s in steered)
    return [STEERED + s.text for s in steered]


class Calls:
    """The tool calls of the prompt ``prompt``, in the table ``table`` of the object's database."""

    def __init__(self, db: Any, table: str, prompt: str) -> None:
        self.db = db
        self.table = table
        self.prompt = prompt
        self.attempt = uuid.uuid4().hex

    async def _table(self) -> None:
        if not await has_table(self.db, self.table):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {self.table} (
                  id TEXT PRIMARY KEY,
                  prompt TEXT NOT NULL,
                  name TEXT NOT NULL,
                  input TEXT NOT NULL,
                  output TEXT,
                  attempt TEXT NOT NULL
                )
            """)

    async def start(self, id: str, name: str, input: dict[str, Any]) -> tuple[str, str | None]:
        """The id of call ``id`` (an earlier attempt's, if it made it), and its output if it finished."""
        await self._table()
        given = json.dumps(input, sort_keys=True)
        earlier = await self.db.query(
            f"SELECT id, output FROM {self.table} WHERE prompt = ? AND name = ? AND input = ? AND attempt != ? "
            "ORDER BY rowid LIMIT 1",
            [self.prompt, name, given, self.attempt],
        )
        if earlier:
            [(id, output)] = earlier
            await self.db.execute(f"UPDATE {self.table} SET attempt = ? WHERE id = ?", [self.attempt, id])
            return id, output
        await self.db.execute(
            f"INSERT INTO {self.table} VALUES (?, ?, ?, ?, NULL, ?)", [id, self.prompt, name, given, self.attempt]
        )
        return id, None

    async def run(
        self, tool: Tool, id: str, input: dict[str, Any], fail: Callable[[BaseException], None]
    ) -> tuple[str, bool]:
        """
        The output of ``tool``'s call ``id``, and whether it is an error. A
        ``restate.TerminalError`` is the tool's answer; a cancellation, or any
        other error, is the attempt's ``fail``ure, which ends the turn.
        """
        try:
            id, output = await self.start(id, tool.name, input)
            if output is None:
                output = text_of(await tool.invoke(input, id))
                await self.db.execute(f"UPDATE {self.table} SET output = ? WHERE id = ?", [output, id])
        except restate.TerminalError as error:
            if error.status_code == 409:
                fail(error)
            return error.message, True
        except Exception as error:
            fail(error)
            return f"Error: {error}", True
        return output, False


def canonical(input: Any) -> str:
    """``input`` as JSON, the same for the same input."""
    return json.dumps(input, sort_keys=True)


def nowhere() -> str:
    """The working directory of an agent without a workspace: an empty directory of its own."""
    path = Path(tempfile.gettempdir()) / "restate-agents" / "no-workspace"
    path.mkdir(parents=True, exist_ok=True)
    return str(path)

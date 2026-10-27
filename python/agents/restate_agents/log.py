"""
An agent's conversation as clients read it: a log in the object's database,
kept in step with the framework's transcript each time the agent publishes a
change (``turns.publish``), and read by ``messages`` and ``sync``.

Each message has an ``id`` that never changes, and a ``seq``: the sequence
number of its last change, which is the cursor. A message that grows in
place (a tool call's output comes) keeps its id and takes a new ``seq``; a
transcript replaced (compacted, reset) leaves its messages in the log, and
the messages after them are new ones. So a client reads from a cursor, and
gets each message that is new or changed since, once.

``clear`` forgets the log with the rest of the conversation, but not where it
got to: the next conversation's ids and cursors go on from there (``START``),
so a client following it reads on, and drops the messages before ``first``.
"""

from __future__ import annotations

import json
from collections.abc import Awaitable, Callable
from typing import Any

from restate_fns.schema import Schema

from restate_agents.common import ChatMessage, Messages, MessagesRequest

TABLE = "restate_agent_log"

# The agent's state key of where its log starts, ``{"seq": cursor, "id": first id}``, past the conversations
# ``clear`` forgot: ``clear`` sets it, none before.
START = "restate-agents.log.start"
SCHEMA = Schema(
    "restate_agents.log",
    [
        f"""CREATE TABLE {TABLE} (
          id INTEGER PRIMARY KEY,
          seq INTEGER NOT NULL UNIQUE,
          position INTEGER UNIQUE,
          message TEXT NOT NULL
        )"""
    ],
)


async def sync(db: Any, messages: list[dict[str, Any]], start: Callable[[], Awaitable[dict[str, int] | None]]) -> None:
    """
    Store the transcript's ``messages`` (each ``{role, parts}``, in order) in
    the log: those that changed since the last time, in one transaction.
    """
    await SCHEMA.ensure(db)
    head = [
        (id, json.loads(message))
        for id, message in await db.query(
            f"SELECT id, message FROM {TABLE} WHERE position IS NOT NULL ORDER BY position"
        )
    ]
    same = 0
    while same < min(len(head), len(messages)) and head[same][1] == messages[same]:
        same += 1
    grown = same
    while grown < min(len(head), len(messages)) and _extends(head[grown][1], messages[grown]):
        grown += 1
    if same == len(head) == len(messages):
        return
    [(seq, next_id)] = await db.query(f"SELECT MAX(seq), MAX(id) + 1 FROM {TABLE}")
    if seq is None:
        begin = await start() or {"seq": 0, "id": 0}
        seq, next_id = begin["seq"], begin["id"]
    async with db.transaction() as tx:
        for position in range(same, grown):
            seq += 1
            await tx.execute(
                f"UPDATE {TABLE} SET seq = ?, message = ? WHERE id = ?",
                [seq, json.dumps(messages[position]), head[position][0]],
            )
        # The rest of the transcript as it was is replaced: its messages stay in the log, out of it.
        await tx.execute(f"UPDATE {TABLE} SET position = NULL WHERE position >= ?", [grown])
        for position in range(grown, len(messages)):
            seq += 1
            await tx.execute(
                f"INSERT INTO {TABLE} VALUES (?, ?, ?, ?)", [next_id, seq, position, json.dumps(messages[position])]
            )
            next_id += 1


async def page(db: Any, request: MessagesRequest | None, start: dict[str, int] | None) -> Messages:
    """
    A page of the log. With ``cursor``: the messages new or changed since it,
    by when they changed, and the cursor to read on from. Without: the latest
    ``limit`` messages (or those before ``before``, an id), oldest first, and
    the cursor to follow the conversation from.
    """
    request = request or {}
    limit = request.get("limit", 50)
    first: dict[str, Any] = {"first": start["id"]} if start else {}
    await SCHEMA.ensure(db)
    if "cursor" in request:
        rows = await db.query(
            f"SELECT id, seq, message FROM {TABLE} WHERE seq > ? ORDER BY seq LIMIT ?", [request["cursor"], limit + 1]
        )
        more = len(rows) > limit
        rows = rows[:limit]
        cursor = rows[-1][1] if rows else request["cursor"]
        return Messages(messages=[_message(id, m) for id, _, m in rows], more=more, cursor=cursor, **first)
    [(cursor,)] = await db.query(f"SELECT COALESCE(MAX(seq), ?) FROM {TABLE}", [start["seq"] if start else 0])
    before = request.get("before")
    rows = await db.query(
        f"SELECT id, message FROM {TABLE} WHERE id < ? ORDER BY id DESC LIMIT ?",
        [before if before is not None else 1 << 62, limit + 1],
    )
    more = len(rows) > limit
    return Messages(messages=[_message(id, m) for id, m in reversed(rows[:limit])], more=more, cursor=cursor, **first)


def _message(id: int, message: str) -> ChatMessage:
    return ChatMessage(id=id, **json.loads(message))


def _extends(old: dict[str, Any], new: dict[str, Any]) -> bool:
    """Whether ``new`` is ``old`` grown in place: the same role and parts, with more (a tool call's output, say)."""
    if old["role"] != new["role"] or len(new["parts"]) < len(old["parts"]):
        return False
    for was, now in zip(old["parts"], new["parts"], strict=False):
        if was["type"] != now["type"]:
            return False
        if was["type"] == "text" and not now["text"].startswith(was["text"]):
            return False
        if was["type"] == "tool" and was["id"] != now["id"]:
            return False
    return True

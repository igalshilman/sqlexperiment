"""
A LangGraph checkpointer on a journal database: ``SqlCheckpointSaver``,
over a database session ``db`` (in Restate, ``ctx.db``), which has ``query``,
``execute`` and ``transaction()``.

Three tables, with values as LangGraph's serializer writes them:

- ``checkpoints``: each checkpoint, without its channel values.
- ``channel_values``: each channel's value at each version, stored when a
  step changes it. A checkpoint's values are those at its channel versions,
  so a step stores only what it changed, not the whole state again (an
  agent's message list, say). Versions are unique (a count and a random
  part): a thread forked from an earlier checkpoint takes the same counts
  again, and its values must not replace those of the branch it forked from.
- ``writes``: the pending writes of each checkpoint's tasks.

Each ``aput`` and ``aput_writes`` is one SQL transaction, so a step is stored
whole, in order with everything else the handler does. Operations run one at
a time, in call order: LangGraph writes the results of parallel tasks at once.
It is asynchronous only, as ``ctx.db`` is.
"""

from __future__ import annotations

import asyncio
import uuid
from collections.abc import AsyncIterator, Sequence
from typing import Any

from langchain_core.runnables import RunnableConfig
from langgraph.checkpoint.base import (
    WRITES_IDX_MAP,
    BaseCheckpointSaver,
    ChannelVersions,
    Checkpoint,
    CheckpointMetadata,
    CheckpointTuple,
    get_checkpoint_id,
    get_checkpoint_metadata,
)
from langgraph.checkpoint.serde.base import SerializerProtocol
from restate_fns import SqlDatabase, SqlSession

from restate_agents.common import has_table

SCHEMA = """
CREATE TABLE IF NOT EXISTS checkpoints (
  thread_id TEXT NOT NULL,
  checkpoint_ns TEXT NOT NULL,
  checkpoint_id TEXT NOT NULL,
  parent_checkpoint_id TEXT,
  type TEXT NOT NULL,
  checkpoint BLOB NOT NULL,
  metadata BLOB NOT NULL,
  PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
);
CREATE TABLE IF NOT EXISTS channel_values (
  thread_id TEXT NOT NULL,
  checkpoint_ns TEXT NOT NULL,
  channel TEXT NOT NULL,
  version TEXT NOT NULL,
  type TEXT NOT NULL,
  value BLOB,
  PRIMARY KEY (thread_id, checkpoint_ns, channel, version)
);
CREATE TABLE IF NOT EXISTS writes (
  thread_id TEXT NOT NULL,
  checkpoint_ns TEXT NOT NULL,
  checkpoint_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  task_path TEXT NOT NULL,
  idx INTEGER NOT NULL,
  channel TEXT NOT NULL,
  type TEXT NOT NULL,
  value BLOB NOT NULL,
  PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
);
"""

COLUMNS = "thread_id, checkpoint_ns, checkpoint_id, parent_checkpoint_id, type, checkpoint, metadata"

# The type of a channel a step emptied.
EMPTY = "empty"


class SqlCheckpointSaver(BaseCheckpointSaver[str]):
    """LangGraph's checkpoints in a SQL database session, such as ``ctx.db``."""

    def __init__(self, db: SqlSession, *, serde: SerializerProtocol | None = None) -> None:
        super().__init__(serde=serde)
        self.db = db
        self._lock = asyncio.Lock()
        self._ready = False

    def get_next_version(self, current: str | int | None, channel: None = None) -> str:
        """
        The version after ``current``: its count plus one, zero-padded so that
        versions compare as their counts do, and a random part, so that
        branches of a thread never share one.
        """
        if current is None:
            count = 0
        elif isinstance(current, int):
            count = current
        else:
            count = int(current.split(".")[0])
        return f"{count + 1:032}.{uuid.uuid4().hex}"

    async def _setup(self) -> None:
        if not self._ready:
            # Created only if missing: a shared handler's ctx.db reads, and creates nothing.
            if not await has_table(self.db, "writes"):
                await self.db.execute(SCHEMA)
            self._ready = True

    async def _channel_values(self, thread_id: str, ns: str, checkpoint: Checkpoint) -> dict[str, Any]:
        """The values of ``checkpoint``'s channels, at its versions."""
        versions = list(checkpoint.get("channel_versions", {}).items())
        if not versions:
            return {}
        rows = await self.db.query(
            "SELECT channel, type, value FROM channel_values WHERE thread_id = ? AND checkpoint_ns = ? AND ("
            + " OR ".join("(channel = ? AND version = ?)" for _ in versions)
            + ")",
            [thread_id, ns, *(item for channel, version in versions for item in (channel, str(version)))],
        )
        return {channel: self.serde.loads_typed((type, value)) for channel, type, value in rows if type != EMPTY}

    async def _tuple(self, row: list[Any]) -> CheckpointTuple:
        thread_id, ns, checkpoint_id, parent_id, type, stored, metadata = row
        checkpoint: Checkpoint = self.serde.loads_typed((type, stored))
        checkpoint = {**checkpoint, "channel_values": await self._channel_values(thread_id, ns, checkpoint)}
        writes = await self.db.query(
            "SELECT task_id, channel, type, value FROM writes WHERE thread_id = ? AND checkpoint_ns = ? "
            "AND checkpoint_id = ? ORDER BY task_path, task_id, idx",
            [thread_id, ns, checkpoint_id],
        )

        def config(id: str) -> RunnableConfig:
            return {"configurable": {"thread_id": thread_id, "checkpoint_ns": ns, "checkpoint_id": id}}

        return CheckpointTuple(
            config=config(checkpoint_id),
            checkpoint=checkpoint,
            metadata=self.serde.loads_typed((type, metadata)),
            parent_config=None if parent_id is None else config(parent_id),
            pending_writes=[
                (task_id, channel, self.serde.loads_typed((write_type, value)))
                for task_id, channel, write_type, value in writes
            ],
        )

    async def aget_tuple(self, config: RunnableConfig) -> CheckpointTuple | None:
        async with self._lock:
            await self._setup()
            configurable = config["configurable"]
            thread_id = configurable["thread_id"]
            ns = configurable.get("checkpoint_ns", "")
            if checkpoint_id := get_checkpoint_id(config):
                rows = await self.db.query(
                    f"SELECT {COLUMNS} FROM checkpoints "
                    "WHERE thread_id = ? AND checkpoint_ns = ? AND checkpoint_id = ?",
                    [thread_id, ns, checkpoint_id],
                )
            else:
                rows = await self.db.query(
                    f"SELECT {COLUMNS} FROM checkpoints WHERE thread_id = ? AND checkpoint_ns = ? "
                    "ORDER BY checkpoint_id DESC LIMIT 1",
                    [thread_id, ns],
                )
            return await self._tuple(rows[0]) if rows else None

    async def alist(
        self,
        config: RunnableConfig | None,
        *,
        filter: dict[str, Any] | None = None,
        before: RunnableConfig | None = None,
        limit: int | None = None,
    ) -> AsyncIterator[CheckpointTuple]:
        async with self._lock:
            await self._setup()
            configurable = (config or {}).get("configurable", {})
            conditions = [
                ("thread_id = ?", configurable.get("thread_id")),
                ("checkpoint_ns = ?", configurable.get("checkpoint_ns")),
                ("checkpoint_id = ?", configurable.get("checkpoint_id")),
                ("checkpoint_id < ?", get_checkpoint_id(before) if before else None),
            ]
            where = [condition for condition, value in conditions if value is not None]
            params = [value for _, value in conditions if value is not None]
            rows = await self.db.query(
                f"SELECT {COLUMNS} FROM checkpoints {'WHERE ' + ' AND '.join(where) if where else ''} "
                "ORDER BY checkpoint_id DESC",
                params,
            )
            found: list[CheckpointTuple] = []
            for row in rows:
                if limit is not None and len(found) >= limit:
                    break
                tuple = await self._tuple(row)
                if all(tuple.metadata.get(key) == value for key, value in (filter or {}).items()):
                    found.append(tuple)
        for tuple in found:
            yield tuple

    async def aput(
        self,
        config: RunnableConfig,
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> RunnableConfig:
        async with self._lock:
            await self._setup()
            configurable = config["configurable"]
            thread_id = configurable["thread_id"]
            ns = configurable.get("checkpoint_ns", "")
            parent_id = configurable.get("checkpoint_id")
            stored = dict(checkpoint)
            values: dict[str, Any] = stored.pop("channel_values")  # type: ignore[assignment]
            type, serialized = self.serde.dumps_typed(stored)
            metadata_type, metadata_value = self.serde.dumps_typed(get_checkpoint_metadata(config, metadata))
            if type != metadata_type:
                raise ValueError("Failed to serialize checkpoint and metadata to the same type.")
            # The channels this step changed, at their new versions.
            changed = [
                (
                    channel,
                    str(version),
                    *(self.serde.dumps_typed(values[channel]) if channel in values else (EMPTY, None)),
                )
                for channel, version in new_versions.items()
            ]
            async with self.db.transaction() as tx:
                for row in changed:
                    await tx.execute(
                        "INSERT OR REPLACE INTO channel_values "
                        "(thread_id, checkpoint_ns, channel, version, type, value) VALUES (?, ?, ?, ?, ?, ?)",
                        [thread_id, ns, *row],
                    )
                await tx.execute(
                    f"INSERT OR REPLACE INTO checkpoints ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    [thread_id, ns, checkpoint["id"], parent_id, type, serialized, metadata_value],
                )
            return {"configurable": {"thread_id": thread_id, "checkpoint_ns": ns, "checkpoint_id": checkpoint["id"]}}

    async def aput_writes(
        self,
        config: RunnableConfig,
        writes: Sequence[tuple[str, Any]],
        task_id: str,
        task_path: str = "",
    ) -> None:
        async with self._lock:
            await self._setup()
            configurable = config["configurable"]
            thread_id = configurable["thread_id"]
            ns = configurable.get("checkpoint_ns", "")
            checkpoint_id = configurable["checkpoint_id"]
            # Special writes (errors, interrupts) replace; others keep the first.
            verb = (
                "INSERT OR REPLACE" if all(channel in WRITES_IDX_MAP for channel, _ in writes) else "INSERT OR IGNORE"
            )
            rows = [
                [thread_id, ns, checkpoint_id, task_id, task_path, WRITES_IDX_MAP.get(channel, idx), channel,
                 *self.serde.dumps_typed(value)]
                for idx, (channel, value) in enumerate(writes)
            ]  # fmt: skip
            async with self.db.transaction() as tx:
                for row in rows:
                    await tx.execute(
                        f"{verb} INTO writes (thread_id, checkpoint_ns, checkpoint_id, task_id, task_path, idx, "
                        "channel, type, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        row,
                    )

    async def adelete_thread(self, thread_id: str) -> None:
        async with self._lock:
            await self._setup()
            async with self.db.transaction() as tx:
                await _delete(tx, thread_id)


async def _delete(db: SqlDatabase, thread_id: str) -> None:
    for table in ("checkpoints", "channel_values", "writes"):
        await db.execute(f"DELETE FROM {table} WHERE thread_id = ?", [thread_id])

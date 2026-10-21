"""An in-memory ordered store, and a database session over it: ``ctx.db`` without Restate."""

import itertools

import restate_sql

_namespaces = itertools.count()


class MemoryStore:
    """A store whose writes are stored at once."""

    def __init__(self) -> None:
        self.values: dict[str, bytes] = {}

    def get(self, key: str) -> bytes | None:
        return self.values.get(key)

    def set(self, key: str, value: bytes) -> None:
        self.values[key] = bytes(value)

    def clear(self, key: str) -> None:
        self.values.pop(key, None)

    def sync(self) -> None:
        pass


async def session(store: MemoryStore | None = None) -> restate_sql.Database:
    """A session of a database of its own (or of ``store``'s), as another process would open it."""
    return await restate_sql.open(f"memory-{next(_namespaces)}", store or MemoryStore())

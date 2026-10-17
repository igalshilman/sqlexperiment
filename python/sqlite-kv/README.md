# Python extension

SQL over a key/value store implemented in Python: a native extension
([PyO3](https://pyo3.rs), built with [maturin](https://www.maturin.rs)) around
`sqlite-kv`, for the Restate Python SDK, where the store is an object's state.
It is the [Node.js addon](../../typescript/packages/sql-node/README.md) for Python, with the same
store, sessions and page cache.

```python
import sqlite_kv

db = await sqlite_kv.open("Ledger/a", store)  # store: get, set, clear, sync
async with db.transaction() as tx:
    await tx.execute("INSERT INTO transfers VALUES(?, ?, ?, ?)", [id, src, dst, amount])
await db.save()  # confirms the session's writes for the page cache
```

## The store

The store is _ordered_, as Restate state writes are: `set` and `clear` return
at once, the store keeps writes in the order they were issued, and a failure
may lose a tail of unconfirmed writes, never one without the writes after it.
`sync` completes once every write so far is stored.

`get` issues its read when it is called, and returns the value (`None` when
absent) or an awaitable of it, as a Restate context's `ctx.get` does. A plain
`async def get` does not do: its read would be issued only when it first runs,
after writes that came later.

Every call to the store is scheduled on the event loop of `open` with
`call_soon_threadsafe`, which runs them in the order they were scheduled: the
store sees them in the order the engine issued them, a write before a later
read of its key, and every write before a `sync`. A statement's result comes
through the same loop, after its writes. A `set` or `clear` that raises fails
the next `sync`.

## Sessions and the page cache

`open` returns a session of the namespace's database. Each SQL commit is
durable once the store has stored its writes (a statement completes once they
reached the store, and `save()` waits until they are stored);
`async with db.transaction() as tx` commits its body as a whole, and rolls it
back when the body raises. Calls run one at a time, in the order they start,
and a transaction holds the session until it commits or rolls back: calls
started meanwhile wait, so none runs inside it. The body uses `tx`, which is
invalid once the body has ended; a call to the session from inside the body
(or a task it started) is refused, as it would wait for the body. A COMMIT
that fails rolls back. An SQL error leaves the session usable
(`is_usable()`); a storage error ends it. `version()` and `record_commit()`
are as in the addon: the version grows with each commit that changes the
database, or that its transaction recorded.

The page cache, shared by the process's databases, is the addon's: 256 MiB of
memory and 1 GiB of disk in a temporary file, unlinked once open, or the file
`SQLITE_KV_CACHE_FILE` names. `save()` ends a session and confirms its writes;
a session that ends otherwise (`close()`, a failed `save`) makes the next
session of that namespace in the process start cold.

## Values

| SQL     | Python                                                    |
| ------- | --------------------------------------------------------- |
| NULL    | `None`                                                    |
| INTEGER | `int` (64 bits); a `bool` parameter is 0 or 1             |
| REAL    | `float`                                                   |
| TEXT    | `str`                                                     |
| BLOB    | `bytes` (`bytearray` and `memoryview` as parameters, too) |

## Build and test

From the repository's root, `make py` builds it into the uv workspace and
`make test-sql-py` runs its tests (pytest, against an in-memory ordered
store): SQL and value round trips, SQL errors and rollback, a write the store
rejects, transactions holding the session, the version, and lost writes, for
every position of a request that loses its writes.

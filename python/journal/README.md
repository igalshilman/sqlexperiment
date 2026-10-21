# Python extension

SQL over a key/value store implemented in Python: a native extension
([PyO3](https://pyo3.rs), built with [maturin](https://www.maturin.rs)) around
the [`journal`](../../core/journal/README.md) crate, for the Restate Python SDK, where the store is an object's state.
It is the [Node.js addon](../../typescript/packages/sql-node/README.md) for Python, with the same
store, sessions and page cache.

```python
import journal

db = await journal.open("Ledger/a", store)  # store: get, set, clear, sync
async with db.transaction() as tx:
    await tx.execute("INSERT INTO transfers VALUES(?, ?, ?, ?)", [id, src, dst, amount])
await db.save()  # ends the session, leaving its engine to the next one
```

## The store

`set` and `clear` return at once, and `sync` completes once every write before
it is stored. The store keeps no order: until a sync completes, a failure may
lose any of the writes before it, and keep any others.

`get` issues its read when it is called, and returns the value (`None` when
absent) or an awaitable of it, as a Restate context's `ctx.get` does. A plain
`async def get` does not do: its read would be issued only when it first runs,
after writes that came later.

Every call to the store is scheduled on the event loop of `open` with
`call_soon_threadsafe`, which runs them in the order they were scheduled, so
a read sees the writes before it. A `set` or `clear` that raises fails the
next `sync`.

## Sessions and the page cache

`open` returns a session of the namespace's database. A statement that commits
completes once its writes are stored;
`async with db.transaction() as tx` commits its body as a whole, and rolls it
back when the body raises. Calls run one at a time, in the order they start,
and a transaction holds the session until it commits or rolls back: calls
started meanwhile wait, so none runs inside it. The body uses `tx`, which is
invalid once the body has ended; a call to the session from inside the body
(or a task it started) is refused, as it would wait for the body. A COMMIT
that fails rolls back. An SQL error leaves the session usable
(`is_usable()`); a storage error ends it. `tx.commit_effects(bytes)` and
`effects()` are as in the addon: effects commit in the same write as the
transaction, and stay until a later one commits others.

The page cache, shared by the process's databases, is the addon's: 256 MiB of
memory and 1 GiB of disk in a temporary file, unlinked once open, or the file
`JOURNAL_CACHE_FILE` names. `save()` (or `close()`) ends a session, leaving
its engine to the next one when it is idle.

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
`make test-py` runs its tests (pytest, against an in-memory store): SQL
and value round trips, SQL errors and rollback, a write the store rejects,
transactions holding the session, effects, and lost writes, for every
position of a request that loses some of its writes.

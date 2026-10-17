# The patched Python SDK

The Python counterpart of [the patched TypeScript SDK](../sdk-typescript/README.md):
the Restate Python SDK with an invocation that does not replay its journal,
and SQL in a virtual object on it, over
[the patched shared core](../sdk-shared-core/README.md), which it links
natively (PyO3) where the TypeScript SDK compiles it to WebAssembly.

|                             | What                                                     | Version                                                                                                                                                                                                                                | Form                                           |
| --------------------------- | -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| [`packages/sdk-python/`](.) | `restate-sdk` (Python) and the shared core it links with | 1.0.5 ([`ad4ea7c0`](https://github.com/restatedev/sdk-python/commit/ad4ea7c0a60b5745681a809fe57656bec0939c38)), shared core [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45) | Patches, applied to fresh clones by `setup.sh` |

## SQL in a virtual object

Each key of an object has its own SQLite-compatible database, kept in the
object's state, through the [`sqlite-kv` extension](../sql-python/README.md):

```python
import restate
from restate import ObjectContext

notes = restate.VirtualObject("notes", **restate.sql_options)


@notes.handler()
async def add(ctx: ObjectContext, note: str) -> None:
    await ctx.db.execute("CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)")
    await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])


@notes.handler()
async def transfer(ctx: ObjectContext, order: dict) -> None:
    async with ctx.transaction() as t:  # one transaction, and the sends it commits with
        await t.db.execute("UPDATE stock SET n = n - ? WHERE sku = ?", [order["qty"], order["sku"]])
        t.tx.object_send(shipping.ship, key=order["sku"], arg=order)
```

It works as in TypeScript ([how SQL in virtual objects works](../sdk-typescript/SQL.md)),
with the same storage, page cache and pending sends:

- `restate.sql_options` is `journal_replay=False`, `enable_lazy_state=True`
  and `journal_retention=timedelta(0)`. `journal_replay` is also a handler
  option, and `ctx.db` needs it off in the handler that uses it.
- `ctx.db` is the object's database (namespace `service/key`), for exclusive
  handlers. Each statement is its own transaction, and
  `async with ctx.db.transaction() as db` groups several. Calls run one at a
  time, in the order they start.
- `async with ctx.transaction() as t` is one transaction, `t.db`, with its
  sends, `t.tx` (`service_send`, `object_send`, `workflow_send`,
  `generic_send`): they are made once it commits, exactly once, even if the
  attempt crashes between the commit and the sends. It commits when the body
  ends and rolls back when it raises.
- An SQL error raises a `TerminalError` (its `__cause__` the SQL error), since
  a retry would repeat it; a storage error, which ends the session, is retried.
- The attempt's session opens on first use and is saved when the handler
  ends: one empty run, if it wrote, so that the page cache may trust it.

## The context without journal replay

With `journal_replay=False`, a retried attempt runs the handler again from the
start, against the state as stored. The context's futures are plain awaitables:
await them, or combine them with `asyncio.gather`, `asyncio.wait`, tasks of
your own, or a library's (`restate.gather` and `restate.select` work too).

- **An operation starts when it is made.** It is driven to completion whether
  or not anything awaits it: a call is made without an `await`.
- **One await point.** One loop per invocation (`restate/selector.py`) drives
  every pending wait and asks the server about all of them at once, so a
  suspension resumes on the first of them to complete.
- **A cancellation reaches every pending wait** (each raises a
  `TerminalError` with status 409); what waits after it runs normally.

On a retried attempt, each operation the handler reaches happens again:

| Operation                         | On a retried attempt                                                                                                                                    |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ctx.db`, `ctx.transaction()`     | Run again, against the state as stored: a handler makes itself idempotent with an id it is given, recorded with its writes                              |
| Calls and sends                   | New ones, unless they have an idempotency key that is the same on every attempt, such as `f"{ctx.request().id}:reserve"`: then they reach the first one |
| `ctx.get`, `ctx.set`, `ctx.clear` | Read the state as stored; writes are ordered, and a lost one takes the later commands with it                                                           |
| `ctx.run`, `ctx.run_typed`        | Not available, a `TerminalError`: its result would not be kept, so call the function directly                                                           |
| `ctx.time()`                      | The time now                                                                                                                                            |
| `ctx.sleep`                       | Starts over: a sleep longer than the inactivity timeout never ends, as each resumption starts it again                                                  |
| Awakeables                        | Refused, a `TerminalError`: use `ctx.signal`                                                                                                            |

## Build

[`setup.sh`](setup.sh) (`make sdk-py`) clones both repositories at the
pinned revisions into `.build/` (ignored), and applies
[`patches/`](patches) and [the shared core's patch](../sdk-shared-core/sdk-shared-core.patch).
The SDK's `Cargo.toml` takes the core from the clone next to it. The uv
workspace ([`pyproject.toml`](../../pyproject.toml)) installs the SDK from
`.build/sdk-python` wherever `restate-sdk` is used, building it with maturin
(`make py`), and again when its Rust or the core's changes. It needs git,
cargo, Python 3.13 and uv. After editing the clones,
[`patches/regenerate.sh`](patches/regenerate.sh) writes the patches back;
`setup.sh` refuses to clone again over edits that the patches do not have.

`make test-sdk-py` runs the SDK's own tests, in its own environment (with the
`sqlite-kv` extension of this repository, for its `sql` extra), except those
against a Restate server in docker (its test harness).

## Changes

| Change                                                                                                                                                                                                                       | Why                                                                                                                                                                                                                                                                                                       | Where                                                                                  |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `JournalReplay::Disabled`, and state commands named after their key                                                                                                                                                          | See [the shared core](../sdk-shared-core/README.md)                                                                                                                                                                                                                                                       | `sdk-shared-core.patch`                                                                |
| A `journal_replay` option of services, objects and workflows, and of their handlers, passed to the VM's constructor                                                                                                          | Turns it off per service, or per handler: a retried attempt then runs the handler again from the start, against the state as stored                                                                                                                                                                       | `service.py`, `object.py`, `workflow.py`, `handler.py`, `server.py`, `vm.py`, `lib.rs` |
| Without journal replay, one loop drives every wait of the invocation, and the context's futures start when they are made                                                                                                     | Concurrency is safe once nothing is replayed, but the protocol has one await point per invocation: a suspension, the awaiting-on hint and a cancellation each take one future. So each wait registers with the selector, which asks the VM about all of them at once, and is the only reader of the input | `selector.py`, `server_context.py`, `server_types.py`                                  |
| Without journal replay, `ctx.run` and awakeables are refused, and `ctx.time()` is the time now                                                                                                                               | With nothing replayed a run's result would not be kept, and signal ids of skipped attempts are unknown                                                                                                                                                                                                    | `server_context.py`                                                                    |
| `ObjectContext.db` and `ObjectContext.transaction()` (SQL and the sends it commits with, pending in the object's state while it commits), `restate.sql_options`, and the `SqlDatabase`, `SqlSession`, `SqlTransaction` types | SQL in a virtual object, through the sqlite-kv extension (the `sql` extra, loaded on first use)                                                                                                                                                                                                           | `sql.py`, `context.py`, `server_context.py`, `__init__.py`                             |
| `restate.sql.set_store_wrapper`                                                                                                                                                                                              | Fault injection in tests                                                                                                                                                                                                                                                                                  | `sql.py`                                                                               |
| `AwaitResponse::EphemeralNotificationReady` is an internal error                                                                                                                                                             | The core's protocol V8 has ephemeral commands, which this SDK does not make                                                                                                                                                                                                                               | `lib.rs`                                                                               |
| Build: the core from the clone next to the SDK (`[patch.crates-io]`); uv `cache-keys` over the Rust; `sqlite-kv` from `packages/sql-python`                                                                                  | The patched core; uv rebuilds the extension when the Rust changes; the SDK's own environment has the extension                                                                                                                                                                                            | `Cargo.toml`, `pyproject.toml`                                                         |

The SDK's tests pass with the patch (38): 3 of the option
(`tests/journal_replay.py`), 10 of the selector against a fake VM
(`tests/selector.py`), and 10 of `ctx.db` and `ctx.transaction` over the
extension and a fake object state (`tests/sql_attempt.py`), among them the
sends of a transaction for every point at which its attempt may crash.

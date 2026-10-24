# restate-fns (Python): SQL in Restate virtual objects

The Python counterpart of [`restate-fns`](../../typescript/packages/fns/README.md),
and the same library: each key of a virtual object gets a SQL database of its
own, `ctx.db`, kept in the object's state, through the
[`restate-sql` extension](../sql/README.md). The handlers that use it run
with the SDK's explicit recovery instead of journal replay. It is a library
over the published Restate Python SDK (1.1.0, pinned: see
[the context](#the-context-with-explicit-recovery)), which it re-exports.

```python
import restate_fns as restate

notes = restate.VirtualObject("notes", schema=["CREATE TABLE notes (text TEXT NOT NULL)"])


@notes.handler()
async def add(ctx: restate.ObjectContext, note: str) -> None:
    await ctx.db.execute("INSERT INTO notes (text) VALUES (?)", [note])


@notes.handler()
async def transfer(ctx: restate.ObjectContext, order: dict) -> None:
    async with ctx.transaction() as t:  # one transaction, and the sends it commits with
        await t.db.execute("UPDATE stock SET n = n - ? WHERE sku = ?", [order["qty"], order["sku"]])
        t.tx.object_send(shipping.ship, key=order["sku"], arg=order)


@notes.handler(kind="shared")
async def count(ctx: restate.ObjectSharedContext) -> int:
    [[n]] = await ctx.db.query("SELECT count(*) FROM notes")
    return n
```

It works as in TypeScript ([how it works](../../typescript/packages/fns/README.md#how-it-works)),
with the same storage, page cache and pending sends:

- `restate_fns` is the SDK (`from restate import *`), with a `VirtualObject`,
  `ObjectContext` and `ObjectSharedContext` of its own. Its `VirtualObject`
  takes the options of the SDK's and adds `restate.sql_options`
  (`enable_lazy_state=True`, `journal_retention=timedelta(0)`). Its handlers
  have `ctx.db`, and run with the SDK's explicit recovery
  (`restate._preview.explicit_recovery`); one with
  `journal_replay=True` is the SDK's own, with no `ctx.db` (its context is
  `restate.JournaledObjectContext`), such as one that waits on an awakeable.
- `VirtualObject(name, schema=[...])` is the steps that make each key's
  database, in order: statements, or async functions of the transaction (to
  move data). They are applied once per key, in one transaction, when a
  handler of the key first opens its database; the table `restate_schema`
  keeps how far each database got, so a step added later (a column, an
  index) runs on each key's database once, as that key is next opened. A
  step never changes once it ran: a change is a step of its own. Tables
  whose names start with `restate_` are Restate's (`restate_schema`, and
  those of the agents of restate_agents): name yours otherwise.
- `ctx.db` is the object's database (namespace `service/key`). Each
  statement is its own transaction, and `async with ctx.db.transaction() as
  db` groups several. Calls run one at a time, in the order they start. A
  key's database holds at most 10 GiB: a statement that would grow it past
  fails as an SQL error ("database or disk is full"), and the session goes
  on.
- In a shared handler, `ctx.db` reads the database as stored, beside the
  exclusive handler that writes it (a chat page reads the conversation while
  an agent answers), and writes nothing: a write statement raises. The
  schema steps its key lacks are applied in its own session, which stores
  nothing: a key never written reads as empty tables. It reads the state as
  of its first call, a snapshot, whatever the exclusive handler commits
  meanwhile; only a shared handler that outlives 16 of the exclusive
  handler's commits fails, and Restate retries it.
- `async with ctx.transaction() as t` is one transaction, `t.db`, with its
  sends, `t.tx` (`service_send`, `object_send`, `workflow_send`,
  `generic_send`): they are made once it commits, exactly once, even if the
  attempt crashes between the commit and the sends. It commits when the body
  ends and rolls back when it raises.
- `await ctx.step(name, body)` runs `body` once for the invocation, such as a
  request to a model: `await ctx.step("ask", lambda: model.ask(question))`. A
  step is a transaction: `body` gets a `SqlTransaction` if it takes an
  argument, and its result (JSON) commits with its SQL and sends. A retried
  attempt that reaches the step gets the stored result without running
  `body`; one that failed before the commit runs it again. The transaction
  begins at the body's first statement, so steps that only make requests run
  concurrently (`asyncio.gather`). A name is one step of the invocation (a
  step in a loop adds what tells its runs apart, `f"ask {turn}"`); the steps
  live in the table `restate_steps`, by the invocation's id, until the
  invocation ends with its result or a terminal error, whose end drops the
  table (an invocation killed without ending leaves its rows to the end of
  the next one that uses the database). Exclusive handlers only.
- An SQL error raises a `TerminalError` (its `__cause__` the SQL error), since
  a retry would repeat it; a storage error, which ends the session, is retried.
- The attempt's session opens on first use and is saved when the handler
  ends: one empty run, if it wrote, so that the page cache may trust it.

[`examples-python`](../../examples-python/README.md) has notes in SQL,
and [`python/tests`](../tests) runs them, a bank, and the context below
against a real Restate server (`make concurrency-test-py`), and the bank
through crashes (`make crash-test-py`).

## The context with explicit recovery

A retried attempt runs the handler again from the start, against the state as
stored. The context's futures are plain awaitables: await them, or combine
them with `asyncio.gather`, `asyncio.wait`, tasks of your own, or a library's
(`restate.gather` and `restate.select` work too).

- **An operation starts when it is made.** It is driven to completion whether
  or not anything awaits it: a call is made without an `await`.
- **One await point.** One loop per invocation,
  [the selector](restate_fns/selector.py), drives every pending wait and asks
  the server about all of them at once, so a suspension resumes on the first
  of them to complete. The SDK drives each awaited future with a loop of its
  own, which concurrent waits would race on: so before a handler with SQL
  runs, the selector takes over its context's waits. That is the SDK's
  internals: the package pins the SDK's version, and refuses to run a
  handler on an SDK without them.
- **A cancellation reaches every pending wait** (each raises a
  `TerminalError` with status 409); what waits after it runs normally.

On a retried attempt, each operation the handler reaches happens again:

| Operation                         | On a retried attempt                                                                                                                                    |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ctx.db`, `ctx.transaction()`     | Run again, against the state as stored: a handler makes itself idempotent with an id it is given, recorded with its writes                              |
| `ctx.step()`                      | Returns the result a previous attempt committed, without running its body; runs it if none did                                                          |
| Calls and sends                   | New ones, unless they have an idempotency key that is the same on every attempt, such as `f"{ctx.request().id}:reserve"`: then they reach the first one |
| `ctx.get`, `ctx.set`, `ctx.clear` | Read the state as stored; writes are ordered, and a lost one takes the later commands with it                                                           |
| `ctx.run`, `ctx.run_typed`        | Run their action again: the result is not kept from one attempt to the next (use `ctx.step` for that). (The store uses an empty run, to sync.)          |
| `ctx.sleep`                       | Starts over: a sleep longer than the inactivity timeout never ends, as each resumption starts it again                                                  |
| Awakeables                        | Refused, a `TerminalError`: use `ctx.signal`                                                                                                            |

## Tests

`make test-py` runs them without a Restate server: the selector against a
fake VM ([`tests/test_selector.py`](tests/test_selector.py)), `ctx.db` and
`ctx.transaction` over the extension and a fake object state
([`tests/test_session.py`](tests/test_session.py)), among them the sends of a
transaction for every point at which its attempt may crash, and the object's
options and handlers
([`tests/test_virtual_object.py`](tests/test_virtual_object.py)).
`restate_fns.session.set_store_wrapper` wraps each object's store, for fault
injection ([`python/tests/faults.py`](../tests/faults.py)).

# Durable functions in Python

Plain Python modules that survive anything: a crash, a restart, a wait of a
month. Each file `<name>.py` is a Restate virtual object `<name>`, and each
function it defines is a handler. While a call waits on a timer, another
service or a model, the interpreter's memory (variables, coroutines, loops
in progress) is saved; when the result comes, or after a crash, it carries
on where it stopped. Nothing is replayed.

```python
# counter.py: the virtual object `counter`, with handlers `add` and `get`
async def add(n, ctx):
    count = (await ctx.kv.get("count") or 0) + n
    await ctx.kv.set("count", count)
    await ctx.sleep(1)  # a durable timer: a crash meanwhile loses nothing
    return count


async def get(_, ctx):
    return await ctx.kv.get("count") or 0
```

These are the [JavaScript functions](../examples-functions/README.md)' twins,
on the same runtime: CPython 3.15 in WebAssembly instead of QuickJS
([how](../rust/functions/docs/PYTHON.md)).

## Run them

```sh
make functions-python                            # from the repository's root; builds CPython for WASI the first time (minutes)
ANTHROPIC_API_KEY=... target/release/functions   # every function here, on port 9080
npx @restatedev/restate-server                   # Restate, in another terminal
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/counter/alice/add --json 20  # 20, a second later
curl localhost:8080/counter/alice/get            # 20
```

The object's key (`alice`) names a scope: calls under the same key share its
SQL database and KV, and run one at a time. Each call starts from the module
as evaluated: what lasts between calls is in `ctx.db` and `ctx.kv`. Each
file's header shows how to call it.

| File                             | What                                                                                          |
| -------------------------------- | --------------------------------------------------------------------------------------------- |
| [`counter.py`](counter.py)       | **Start here.** A count per key, and a durable one-second timer                               |
| [`notes.py`](notes.py)           | Notes in SQL, with t-strings: `ctx.db.sql(t"... {value}")`                                    |
| [`refunds.py`](refunds.py)       | A refund that waits for a manager's approval (`ctx.signal`), and the refunds granted in SQL   |
| [`researcher.py`](researcher.py) | An AI agent (Claude) that studies a question from three angles at once, with `asyncio.gather` |

## Writing a function

Define a function `(input, ctx)`; it may be async. Every function the module
defines is a handler, but those whose names start with `_` (or, if the module
has an `__all__`, those it names). The input and the result are JSON (no
input is `None`). `ctx` is all it needs from outside. It runs on asyncio: `asyncio.gather`,
`asyncio.wait` and task groups work as usual, and so do timers, which are
durable here (`asyncio.sleep`, `asyncio.timeout`, `asyncio.wait_for`).

| Call                                                   | What                                                                                           |
| ------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `ctx.db.sql(t"SELECT ... {value}")`                    | The scope's SQL database: rows as dicts. A t-string's `{}` values are parameters. Synchronous. |
| `with ctx.db.transaction():`                           | The block's statements together, or none if it raises                                          |
| `await ctx.kv.get/set/delete(key)`                     | The scope's JSON values                                                                        |
| `await ctx.rpc.call(service, handler, input, key=...)` | Any Restate service                                                                            |
| `await ctx.fetch(url, method=, headers=, json=)`       | An HTTP request, sent by the host: only where it allows, with the API keys it adds             |
| `await ctx.sleep(seconds)`                             | A durable timer, however long                                                                  |
| `await ctx.signal(name, input)`                        | A value from outside, such as a person's approval: the object's `signal` handler gives it      |
| `ctx.key`                                              | The object's key                                                                               |

- It is Python 3.15 with its standard library, and no other packages. All
  I/O goes through `ctx`: there are no sockets, files to write or
  subprocesses. `print` goes to the server's log.
- SQL commits with the function's memory, together: no transaction is needed
  to stay consistent. `ctx.db.transaction()` is for undoing.
- The call ends when its handler returns: tasks still pending are dropped.
  Await what must happen.
- The input of `ctx.rpc.call` or `ctx.call` stays in the handler's memory
  until the call is answered, and is read from there when the call is made,
  and again after a crash: nothing else stores it. Do not change it until
  then. To set a call's idempotency key, pass `idempotency_key=...`: the
  service called runs once for every call with that key.
- Every object also has `pending`, the signals its calls wait for, and
  `signal`, which gives one, by its wait's `id` or to the oldest wait for its
  `name` (as for [the JavaScript functions](../examples-functions/README.md#writing-a-function)).
  Handlers cannot be named `pending` or `signal`.
- A failed operation raises `OperationError`; an exception a handler raises
  fails the call, with its type and message.
- A changed file is a new function: a call still waiting when its file
  changes cannot carry on, since its memory belongs to the old source.

# restate-agents (Python)

Agent frameworks in Restate virtual objects, in Python: each object (one per
key) keeps the agent's state in its SQL database (`ctx.db`, of
[the patched SDK](../sdk/README.md)), and each message is answered once per
invocation, however often an attempt crashes. The Python counterpart of
[`restate-agents`](../../packages/integrations/README.md); for now, with
LangGraph.

```python
from restate_agents import langgraph, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"


assistant = langgraph.agent("assistant", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

```sh
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/messages   # the conversation so far
```

## LangGraph

`langgraph.agent(name, model=, system=, tools=)` is a virtual object, one per
key (its thread), with the handlers `chat` (a message in, the answer out; as
plain text, `-d 'Hi'`, or a JSON string, `--json '"Hi"'`) and `messages` (the
conversation so far, with the tool calls, a page at a time:
`{"limit": 50, "before": id, "after": id}`). Add handlers of your own with
`@assistant.handler()`.

| Option   | What                                                                                                                                                                                                                        |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `anthropic/model-id` (`ChatAnthropic`); or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`); or any LangChain chat model that calls tools; or a function of the invocation's context giving either |
| `system` | The system prompt                                                                                                                                                                                                           |
| `tools`  | LangChain tools, or functions of the invocation's context giving them                                                                                                                                                       |

The graph is the usual tool-calling loop: a model node, a tools node, and
back to the model until it answers. Its checkpoints are in the object's
database ([`checkpointer.py`](restate_agents/langgraph/checkpointer.py),
`SqlCheckpointSaver`): checkpoints, channel values and pending writes in
three tables, a step storing only the channels it changed.

### Tools

`@tool` makes a tool of a function: the model gives its parameters, which
their types and the docstring describe. Two parameters are not the model's:
`ctx`, the handler's context (`ctx.db`, calls, sends), and `call_id`, the
tool call's id. A crash runs an unfinished tool call again with the same id,
so a tool that must not run twice passes it on, as an idempotency key or a
row's key ([`examples-py/teller.py`](../../examples-py/teller.py)):

```python
@tool
async def send_money(ctx, call_id, to: str, amount: int) -> str:
    """Send money from the customer's account to another account."""
    left = await ctx.object_call(transfer, key=ctx.key(), arg={"id": call_id, "to": to, "amount": amount})
    return f"Sent {amount} to {to}, {left} left"
```

LangChain's own tools work too.

### Through crashes

The handler is not replayed: a retried attempt runs it again from the start,
and LangGraph carries on from the last step it stored.

- **Each step is stored before the next starts** (`durability="sync"`). So
  the tool calls a model made are stored before any tool runs, and a crash
  during the tools resumes them with the same tool call ids, without asking
  the model again.
- **A tool's error fails the attempt**, and Restate retries it; but a
  `restate.TerminalError`, which a retry would repeat, is the tool's answer to
  the model.
- **A message is answered once per invocation**: the graph records each
  answer in its state, by invocation id, and a retried attempt returns it. A
  run that a crash interrupted (its prompt without an answer) is finished
  before the next message starts.

## Objects

`Object(name, schema=...)` is a virtual object with a SQL database of its
own, `ctx.db`, one per key: `restate.VirtualObject` with
`restate.sql_options`, retries up to 100,000 times, 50 to 500 ms apart (each
attempt resumes from the database), and the SQL storage hidden from the
journal in Restate's UI. Each handler first runs `schema`, such as `CREATE
TABLE IF NOT EXISTS`. A handler takes `ctx`, then its input: one parameter,
or several, which a JSON object gives by name:

```python
account = Object("account", schema="CREATE TABLE IF NOT EXISTS moves (id TEXT PRIMARY KEY, amount INTEGER)")


@account.handler
async def deposit(ctx, id: str, amount: int) -> int:
    await ctx.db.execute("INSERT OR IGNORE INTO moves VALUES (?, ?)", [id, amount])
    [[balance]] = await ctx.db.query("SELECT sum(amount) FROM moves")
    return balance
```

An agent is an `Object` too: `langgraph.agent(name, schema=..., ...)` takes
its options, and `@agent.handler` adds handlers of your own
([`examples-py/memory.py`](../../examples-py/memory.py)).
`serve(*services)` serves them on `PORT`, or 9080.

## Tests

`make test-agents-py`, from the repository's root:

- [`tests/test_checkpointer.py`](tests/test_checkpointer.py): LangGraph's
  checkpointer conformance suite (`langgraph-checkpoint-conformance`, its 58
  base tests), over the extension and an in-memory store.
- [`tests/test_langgraph.py`](tests/test_langgraph.py): the agent on a
  scripted model (the teller, [`tests/fakes.py`](tests/fakes.py)), over a fake
  object state: the thread, an answer once per invocation, a crash in a tool,
  a crash at each state request of a message (with `durability="async"`, it
  moves money twice), an interrupted run, the system prompt, and tools'
  errors.

Against a real Restate server ([`../tests`](../tests)): `make
concurrency-test-py` has the agent moving money through Restate calls, and
`make crash-test-py` prompts it while the process crashes, checking that each
prompt moved money exactly once.

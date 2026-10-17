# restate-agents (Python)

Agent frameworks in Restate virtual objects, in Python: each object (one per
key) keeps the agent's state in its SQL database (`ctx.db`, of
[the patched SDK](../sdk/README.md)), and each message is answered once per
invocation, however often an attempt crashes. The Python counterpart of
[`restate-agents`](../../packages/integrations/README.md), with
[LangGraph](#langgraph) and [Pydantic AI](#pydantic-ai).

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
`{"limit": 50, "before": id, "after": id}`); `status`, `stop` and `steer`
([Turns](#turns)); and `pending` and `signal` ([Restate's tools](#restates-tools)).
Add handlers of your own with `@assistant.handler`.

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

### A graph of your own

`langgraph.agent(name, graph=...)` runs a `StateGraph` of yours instead
(uncompiled: the agent compiles it with the object's database as its
checkpointer), or a function of the invocation's context giving one. Its
state has `messages` (`MessagesState`, or a subclass with fields of your
own, stored with the thread); each message goes in as a `HumanMessage`, and
the answer is the text of the last `AIMessage`. Its nodes get the handler's
context from `restate_agents.context()`, and run their tools with
`langgraph.tool_node(tools)`: LangGraph's `ToolNode`, for `@tool`s, Restate's
tools and LangChain's, which then takes the messages steered to the turn
(and a node's model binds them with
`model.bind_tools(langgraph.langchain_tools(tools))`)
([`examples-py/triage.py`](../../examples-py/triage.py),
[`examples-py/writer.py`](../../examples-py/writer.py)):

```python
graph = StateGraph(MessagesState)
graph.add_node("model", call_model)
graph.add_node("tools", langgraph.tool_node([get_weather]))
graph.add_edge(START, "model")
graph.add_conditional_edges("model", tools_condition)
graph.add_edge("tools", "model")

assistant = langgraph.agent("assistant", graph=graph)
```

`langgraph.prompt(ctx, graph, text)` answers a prompt in a handler of your
own, as `chat` does: once, however often an attempt crashes (an
invocation's first prompt has its id, the next `<id>:2`, and so on).
LangGraph's `interrupt()` does not work here: wait for a person with a
`signal` tool.

### Tools

`@tool` makes a tool of a function: the model gives its parameters, which
their types and the docstring describe. Three parameters are not the model's:
`ctx`, the handler's context (`ctx.db`, calls, sends), `call_id`, the tool
call's id, and `idempotency_key`, the call's in this invocation
(`<invocation id>:<call id>`). A crash runs an unfinished tool call again
with the same id, so a tool that must not run twice passes it on, as an
idempotency key or a row's key:

```python
@tool
async def send_money(ctx, idempotency_key, to: str, amount: int) -> str:
    """Send money from the customer's account to another account."""
    await ctx.object_call(transfer, key=ctx.key(), arg={"to": to, "amount": amount}, idempotency_key=idempotency_key)
    return f"Sent {amount} to {to}"
```

LangChain's own tools work too.

### Restate's tools

Tools that call, send to, start and wait for what Restate runs, each once
per tool call, however often an attempt crashes (its idempotency key is the
tool call's), as in [`examples-py/support.py`](../../examples-py/support.py):

```python
from restate_agents import call, langgraph, send, signal, subagent, workflow

support = langgraph.agent(
    "support",
    tools=[
        call(status),  # a handler of a service, an object or a workflow
        signal("approval", description="Wait for a manager to approve a refund.", input=Refund),
        workflow(refund),  # a restate.Workflow, run to its end
        subagent(researcher, description="a researcher, who answers with facts"),
        send(checkin, delay=timedelta(days=1)),
    ],
)
```

| Tool                                                   | What                                                                                                                                                                                                                                                                                                      |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `call(handler, description=, name=, key=)`             | Calls `handler` and answers with what it returns. The model gives its input (its parameter, or its pydantic model's fields) and, for an object or a workflow, the key, unless `key` (a string, or a function of the context) does. `name`: `<service>_<handler>`; `description`: the handler's docstring. |
| `send(handler, description=, name=, key=, delay=)`     | Sends to `handler`, after `delay` if given, and does not wait.                                                                                                                                                                                                                                            |
| `workflow(wf, description=, name=, key=)`              | Runs the workflow `wf`, keyed `<this key>-<call id>` unless `key` keys it, and answers with its result.                                                                                                                                                                                                   |
| `subagent(agent, description=, name=, handler="chat")` | Hands a named task to `agent`, keyed `<this key>.<task name>`: tasks of different names run at once, the same name continues the same task.                                                                                                                                                               |
| `signal(signal, description=, input=, name=)`          | Waits for the signal `signal` to the agent, without holding a process, and answers with its value. With `input` (a pydantic model), the model says what it waits for.                                                                                                                                     |

A handler's `restate.TerminalError` is the tool's answer to the model. Each
agent has two more handlers, for its signals: `pending`, the waits, and
`signal`, which resolves one by its id, or the oldest for a name
(`{"name": "approval", "value": "approved"}`; with `"keep": true`, kept for
the next wait if none waits). The waits are in a journaled virtual object,
`signals`, which `serve` (and `app`) serve with your services.

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
- **A message is answered once per invocation**: it goes into the thread
  with its invocation's id as its own, and a retried attempt that finds it
  there returns its answer. A run that a crash cut off (with steps left) is
  finished before the next message starts.

## Pydantic AI

`pydantic_ai.agent(name, model=, instructions=, tools=)` is the same virtual
object (`chat`, `messages`, `status`, `stop`, `steer`, `pending`, `signal`)
on [Pydantic AI](https://ai.pydantic.dev):

```python
from restate_agents import serve, tool
from restate_agents.pydantic_ai import agent

assistant = agent("assistant", instructions="Answer briefly.", tools=[get_weather])
serve(assistant)
```

| Option         | What                                                                                                                                                                                                                   |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`        | `provider/model-id` (any of Pydantic AI's providers whose package is installed: `anthropic` is), a Pydantic AI model, or a function of the context giving one. Default: `MODEL`, or else `anthropic/claude-sonnet-5-5` |
| `instructions` | The instructions                                                                                                                                                                                                       |
| `tools`        | `tool`s, Restate's tools, and Pydantic AI's tools or tool functions (`RunContext` and all)                                                                                                                             |

Or bring an `Agent` of your own, whose tools include Restate's through
`toolset(tools)`; and ask it from handlers of your own with `prompt(ctx,
agent, text, **run)` (`run`: `Agent.run`'s options, such as `output_type` or
`deps`), each prompt answered once
([`examples-py/trips.py`](../../examples-py/trips.py)):

```python
from pydantic_ai import Agent
from restate_agents.pydantic_ai import agent, prompt, toolset

planner = Agent("anthropic:claude-sonnet-5-5", tools=[weather], toolsets=[toolset([signal("approval")])])
trips = agent("trips", planner)


@trips.handler
async def plan(ctx) -> Plan:
    return await prompt(ctx, planner, "Write down the trip we planned, day by day.", output_type=Plan)
```

Through crashes, as with LangGraph: the conversation (a row per message, in
`pydantic_ai_messages`) is stored before each model request and with each
response, so a retried attempt carries on from there: the tool calls a model
made run again with the same ids, without asking the model again; the
message it answers is in it, marked with the invocation's id, and a retried
attempt that finds it answered returns the answer (a structured output too)
without asking the model again. A tool's `restate.TerminalError` is its
answer to the model; any other error fails the attempt.

## Turns

The message `chat` answers is a turn. While it answers, `status` says so
(`{"answering": {"invocationId": "inv_1f…", "since": …}, "waiting": [...]}`,
with what its tools wait for); `stop` stops it, and `steer` adds to it:

```sh
curl localhost:8080/assistant/alice/status
curl localhost:8080/assistant/alice/stop                              # {"stopped": "inv_1f…"}
curl localhost:8080/assistant/alice/steer -d 'Only Monday, please'    # {"invocationId": "inv_1f…", "steered": true}
```

- **`stop`** cancels the turn's invocation (Restate's cancellation, as its
  admin API cancels one): its tools' calls and waits are cancelled with it,
  each cut-off tool call and the turn are answered "Cancelled", and the next
  message starts afresh. It reaches the turn at its next Restate operation
  (a tool's call or wait, a storage request); during a model request, once
  the request is answered.
- **`steer`** adds a message to the turn: the agent takes it after its next
  tool call (LangGraph's `tool_node`; in Pydantic AI, before the next model
  request), and the model reads it with the tools' results.
  A turn that ends before (no tool call came) answers it as a message of its
  own; with no turn, `steer` is a message of its own (`"steered": false`, and
  its invocation, to attach to). A stopped turn answers none of them. The
  steered messages wait in the agent's `signals` object, and go into the
  thread by their ids: a retried attempt neither loses nor repeats one.

## Handlers of your own

An agent is a virtual object with a SQL database of its own, `ctx.db`, one
per key: `restate.VirtualObject` with `restate.sql_options`, retries up to
100,000 times, 50 to 500 ms apart (each attempt resumes from the database),
and the SQL storage hidden from the journal in Restate's UI.
`@assistant.handler` adds a handler, which takes `ctx`, then its input: one
parameter, or several, which a JSON object gives by name
([`examples-py/memory.py`](../../examples-py/memory.py)). `serve(*services)`
serves them on `PORT`, or 9080; with `background=True`, on a thread of its
own, returning at once, as in a Modal Server
([`examples-py/on_modal.py`](../../examples-py/on_modal.py)).

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
- [`tests/test_pydantic_ai.py`](tests/test_pydantic_ai.py): the same, for
  the Pydantic AI agent (the teller as a Pydantic AI model), and agents of
  your own, prompts of your own and a structured output, each answered once.
- [`tests/test_tools.py`](tests/test_tools.py): Restate's tools on a fake
  context: their names, the model's input, and the calls they make.

Against a real Restate server ([`../tests`](../tests)): `make
concurrency-test-py` has the agent moving money through Restate calls, and
Restate's tools on LangGraph and on Pydantic AI
([`test_restate_tools.py`](../tests/test_restate_tools.py): a call, a signal
and a workflow in one turn, signals kept and dropped, a cancelled wait, a
subagent, a send, status, stop and steer), and
`make crash-test-py` prompts it while the process crashes, checking that each
prompt moved money exactly once.

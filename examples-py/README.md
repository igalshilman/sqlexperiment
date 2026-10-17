# Python examples

Each one is a file you run with [uv](https://docs.astral.sh/uv/), from here,
after `make py` at the repository's root. See [Run an example](#run-an-example).

| File                       | What                                                                                                           |
| -------------------------- | -------------------------------------------------------------------------------------------------------------- |
| [`notes.py`](notes.py)     | SQL in a virtual object, with `ctx.db`                                                                         |
| [`chat.py`](chat.py)       | A durable chat agent on LangGraph, with a tool                                                                 |
| [`memory.py`](memory.py)   | An assistant that remembers facts about you in its own SQL table, through its tools, with handlers of your own |
| [`support.py`](support.py) | A support agent whose tools are Restate itself: a service, a manager's approval (a signal), a workflow         |

They are built from [`restate_agents`](../python/agents/README.md): an agent
is a virtual object per conversation, and its tools are plain functions.

```python
from restate_agents import langgraph, serve, tool


@tool
async def remember(ctx, fact: str) -> str:
    """Remember a fact about the user."""  # the model reads this
    ...


assistant = langgraph.agent("assistant", tools=[remember])  # chat and messages handlers
serve(assistant)
```

A tool's `ctx` and `call_id` parameters are not the model's: the handler's
context (`ctx.db`, calls, sends), and the tool call's id, the same when a
crash runs the call again. Restate's own tools call your services, objects,
workflows and other agents, and wait for signals:

```python
support = langgraph.agent("support", tools=[call(status), signal("approval", description=...), workflow(refund)])
```

`notes.py` uses the patched SDK alone, with
`restate.sql_options`.

## Run an example

```sh
ANTHROPIC_API_KEY=… uv run chat.py                                   # the agent, on port 9080
npx @restatedev/restate-server                                        # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080   # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/messages                          # the conversation so far
```

The agents take `MODEL` (`anthropic/<model-id>`), or else Claude Sonnet,
with `ANTHROPIC_API_KEY`; `notes.py` needs no key. Each
example's docstring has its requests.

# Python examples

Each one is a file you run with [uv](https://docs.astral.sh/uv/), from here,
after `make py` at the repository's root. See [Run an example](#run-an-example).

| File                     | What                                                                                                           |
| ------------------------ | -------------------------------------------------------------------------------------------------------------- |
| [`notes.py`](notes.py)   | SQL in a virtual object, with `ctx.db`                                                                         |
| [`chat.py`](chat.py)     | A durable chat agent on LangGraph, with a tool                                                                 |
| [`memory.py`](memory.py) | An assistant that remembers facts about you in its own SQL table, through its tools, with handlers of your own |

They are built from three things of [`restate_agents`](../python/agents/README.md):

```python
from restate_agents import Object, langgraph, serve, tool

notes = Object("notes", schema="CREATE TABLE IF NOT EXISTS notes (text TEXT)")  # a database per key


@notes.handler
async def add(ctx, note: str):
    await ctx.db.execute("INSERT INTO notes VALUES (?)", [note])


@tool
async def remember(ctx, fact: str) -> str:
    """Remember a fact about the user."""  # the model reads this
    ...


assistant = langgraph.agent("assistant", tools=[remember])  # chat and messages handlers
serve(notes, assistant)
```

A handler takes `ctx`, then its input: one parameter, or several, which a
JSON object gives by name (`--json '{"title": "Milk", "done": false}'`). A tool's
`ctx` and `call_id` parameters are not the model's: the handler's context,
and the tool call's id, the same when a crash runs the call again.

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

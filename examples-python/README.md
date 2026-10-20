# Python examples

Each one is a file you run with [uv](https://docs.astral.sh/uv/), from here,
after `make py` at the repository's root. See [Run an example](#run-an-example).

| File                                     | What                                                                                                                 |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| [`chat.py`](chat.py)                     | **Start here.** A durable chat agent on Claude (the Anthropic SDK), with a tool                                      |
| [`chat_langgraph.py`](chat_langgraph.py) | The same agent on LangGraph                                                                                          |
| [`notes.py`](notes.py)                   | SQL in a virtual object, with `ctx.db`                                                                               |
| [`memory.py`](memory.py)                 | An assistant that remembers facts about you in its own SQL table, through its tools, with handlers of your own       |
| [`on_modal.py`](on_modal.py)             | The support agent on [Modal](https://modal.com), as a Modal Server                                                   |
| [`support.py`](support.py)               | A support agent whose tools are Restate itself: a service, a manager's approval (a signal), a workflow               |
| [`trips.py`](trips.py)                   | A travel agent on Pydantic AI: an Agent of your own, an approval before booking, and its plan as typed output        |
| [`triage.py`](triage.py)                 | A LangGraph graph of your own: a router hands each message to the billing or the tech team, each with its tools      |
| [`writer.py`](writer.py)                 | A LangGraph graph of your own: a writer drafts and a model edits, a person approves (a signal), a workflow publishes |

They are built from [`restate_agents`](../python/agents/README.md): an agent
is a virtual object per conversation, and its tools are plain functions.

```python
from restate_agents import claude, serve, tool


@tool
async def remember(ctx, fact: str) -> str:
    """Remember a fact about the user."""  # the model reads this
    ...


assistant = claude.agent("assistant", tools=[remember])  # chat and messages handlers
serve(assistant)
```

A tool's `ctx` and `call_id` parameters are not the model's: the handler's
context (`ctx.db`, calls, sends), and the tool call's id, the same when a
crash runs the call again. Restate's own tools call your services, objects,
workflows and other agents, and wait for signals:

```python
support = claude.agent("support", tools=[call(status), signal("approval", description=...), workflow(refund)])
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

## Run on Modal

[`on_modal.py`](on_modal.py) deploys `support.py` to Modal, as a Modal Server
(`@app.server(h2_enabled=True)`), which serves the agent in its own process
(`serve(..., background=True)`): Restate calls it over HTTP/2, end to end.
The agents need that: an attempt that waits runs its handler again from the
start, so HTTP/1.1's request-response mode (Modal's web endpoints, AWS
Lambda) cannot serve them. Its image installs the wheels of `make wheels`
(the patched SDK and the SQL extension are not on PyPI).

```sh
make wheels                                          # at the repository's root: dist/
uvx modal secret create anthropic ANTHROPIC_API_KEY=…
uvx modal deploy on_modal.py
restate deployments register https://<workspace>--restate-support-support.us-east.modal.direct \
    --extra-header "Modal-Key: wk-…" --extra-header "Modal-Secret: ws-…"   # a Modal proxy token
```

Not tried on Modal yet: whether its Servers stream HTTP/2 both ways at once,
and whether a long request (an agent's turn) has a limit there.

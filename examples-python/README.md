# Python examples

Each one is a file you run with [uv](https://docs.astral.sh/uv/), from here,
after `make py` at the repository's root. See [Run an example](#run-an-example).

| File                                     | What                                                                                                                                                                     |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [`chat.py`](chat.py)                     | **Start here.** A durable chat agent on Claude (the Anthropic SDK), with a tool                                                                                          |
| [`chat_langgraph.py`](chat_langgraph.py) | The same agent on LangGraph                                                                                                                                              |
| [`chat_litellm.py`](chat_litellm.py)     | The same agent on any model (OpenAI's, Gemini, Ollama, ...), through LiteLLM                                                                                             |
| [`memory.py`](memory.py)                 | An assistant that remembers facts about you in its own SQL table, through its tools, with handlers of your own                                                           |
| [`support.py`](support.py)               | A support agent whose tools are Restate itself: a service, a manager's approval (a signal), a workflow                                                                   |
| [`follow.py`](follow.py)                 | A conversation followed from a terminal as the agent works (`restate_agents.client`); a chat page for it is [`examples-typescript/ui`](../examples-typescript/ui/app.ts) |
| [`followup.py`](followup.py)             | An agent that schedules a check-in for a day later: `send` with a delay                                                                                                  |
| [`harness.py`](harness.py)               | An agent each user configures: its model and persona from the object's database, and `compact` and `reset` handlers                                                      |
| [`team.py`](team.py)                     | A team on three frameworks: an editor (Claude) hands research to a researcher (LangGraph) and drafting to a writer (Pydantic AI)                                         |
| [`router.py`](router.py)                 | A front desk of your own: a handler asks a model which team handles a message (`claude.prompt`), then asks that team's agent (`ask`)                                     |
| [`trips.py`](trips.py)                   | A travel agent on Pydantic AI: an Agent of your own, an approval before booking, and its plan as typed output                                                            |
| [`triage.py`](triage.py)                 | A LangGraph graph of your own: a router hands each message to the billing or the tech team, each with its tools                                                          |
| [`writer.py`](writer.py)                 | A LangGraph graph of your own: a writer drafts and a model edits, a person approves (a signal), a workflow publishes                                                     |
| [`notes.py`](notes.py)                   | SQL in a virtual object, with `ctx.db`                                                                                                                                   |
| [`on_modal.py`](on_modal.py)             | The support agent on [Modal](https://modal.com), as a Modal Server                                                                                                       |

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

`notes.py` uses [`restate-fns`](../python/fns/README.md) alone:
`restate_fns.VirtualObject`.

## Run an example

```sh
ANTHROPIC_API_KEY=… uv run chat.py                                   # the agent, on port 9080
npx @restatedev/restate-server                                        # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080   # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/messages                          # the conversation so far
```

The agents take `MODEL` (`anthropic/<model-id>`), or else Claude Sonnet,
with `ANTHROPIC_API_KEY`; `chat_litellm.py` takes any of LiteLLM's models
(`MODEL=openai/gpt-5`, with `OPENAI_API_KEY`); `notes.py` needs no key. Each
example's docstring has its requests.

## Run on Modal

[`on_modal.py`](on_modal.py) deploys `support.py` to Modal, as a Modal Server
(`@app.server(h2_enabled=True)`), which serves the agent in its own process
(`serve(..., background=True)`): Restate calls it over HTTP/2, end to end.
The agents need that: an attempt that waits runs its handler again from the
start, so HTTP/1.1's request-response mode (Modal's web endpoints, AWS
Lambda) cannot serve them. Its image installs the wheels of `make wheels`
(the SQL extension, restate-fns and restate-agents are not on PyPI; the
Restate SDK is, and pip installs it with them).

```sh
make wheels                                          # at the repository's root: dist/
uvx modal secret create anthropic ANTHROPIC_API_KEY=…
uvx modal deploy on_modal.py
restate deployments register https://<workspace>--restate-support-support.us-east.modal.direct \
    --extra-header "Modal-Key: wk-…" --extra-header "Modal-Secret: ws-…"   # a Modal proxy token
```

Not tried on Modal yet: whether its Servers stream HTTP/2 both ways at once,
and whether a long request (an agent's turn) has a limit there.

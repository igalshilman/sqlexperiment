# restate-agents (Python)

Agent frameworks in Restate virtual objects, in Python: each object (one per
key) keeps the agent's state in its SQL database (`ctx.db`, of
[`restate-fns`](../fns/README.md)), and each message is answered once per
invocation, however often an attempt crashes. The Python counterpart of
[`restate-agents`](../../typescript/packages/integrations/README.md).

```python
from restate_agents import claude, serve, tool


@tool
def get_weather(city: str) -> str:
    """The current weather in a city."""
    return f"Sunny and 21°C in {city}"


assistant = claude.agent("assistant", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

```sh
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/messages   # the conversation so far
```

## Choose a framework

The same agent runs on each: `<framework>.agent(name, model=, system=,
tools=)`, with the same handlers (`chat`, `messages`, `status`, `stop`,
`steer`, `pending`, `signal`, `sync`, `notifications`, `watch`) and Restate's tools.
Where the framework has agents of its own (LangGraph, Pydantic AI, the
OpenAI Agents SDK, ADK), yours can go in its place, with
`<framework>.tools([...])` for Restate's tools. `<framework>.prompt(ctx, ...)`
answers a prompt in a handler of your own, once.

| Import                                | Framework                          | Choose it when                                                                | Its example                                                               |
| ------------------------------------- | ---------------------------------- | ----------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| [`claude`](#claude)                   | The Anthropic SDK                  | You want Claude with nothing in between: the lightest, the API's own messages | [`chat.py`](../../examples-python/basic/chat.py)                          |
| [`litellm`](#litellm)                 | LiteLLM                            | You want the same, on any model: OpenAI's, Gemini, Bedrock, a local one       | [`release_notes.py`](../../examples-python/intermediate/release_notes.py) |
| [`langgraph`](#langgraph)             | LangGraph                          | You use LangChain, or want a graph of your own, its steps Restate's           | [`writer.py`](../../examples-python/intermediate/writer.py)               |
| [`pydantic_ai`](#pydantic-ai)         | Pydantic AI                        | You want typed outputs, and an `Agent` of your own                            | [`trips.py`](../../examples-python/intermediate/trips.py)                 |
| [`openai_agents`](#openai-agents-sdk) | The OpenAI Agents SDK              | You want handoffs between agents, guardrails, OpenAI's models                 | [`airline.py`](../../examples-python/intermediate/airline.py)             |
| [`adk`](#google-adk)                  | Google's Agent Development Kit     | You want Gemini, ADK's sub-agents and callbacks                               | [`oncall.py`](../../examples-python/intermediate/oncall.py)               |
| [`claude_agent`](#claude-code)        | Claude Code (the Claude Agent SDK) | You want a coding agent: Claude Code's own tools in a workspace               | [`coder.py`](../../examples-python/intermediate/coder.py)                 |
| [`codex`](#codex)                     | Codex (`codex app-server`)         | You want OpenAI's coding agent: its tools in a workspace, in its sandbox      | [`coder_codex.py`](../../examples-python/intermediate/coder_codex.py)     |

`system` is the whole system prompt for Claude, LiteLLM, LangGraph, Pydantic
AI, the OpenAI Agents SDK and ADK, and is added to the framework's own for
Claude Code and Codex. `model` is `provider/model-id`; without one, an
agent takes `MODEL`, or its framework's default.

## Claude

`claude.agent(name, model=, system=, tools=)` is a virtual object, one per
key (its conversation), on the [Anthropic SDK](https://github.com/anthropics/anthropic-sdk-python):
Claude with nothing in between, the API's own messages stored in the
object's database. It has the handlers `chat` (a message in, the answer out;
as plain text, `-d 'Hi'`, or a JSON string, `--json '"Hi"'`) and `messages`
(the conversation so far, with the tool calls, a page at a time:
`{"limit": 50, "before": id}`, or what is new since a cursor: `{"cursor": n}`); `status`, `stop` and `steer`
([Turns](#turns)); `pending` and `signal` ([Restate's tools](#restates-tools));
and `sync`, `notifications` and `watch` ([Follow a conversation](#follow-a-conversation)).
Add handlers of your own with `@assistant.handler`.

| Option    | What                                                                                                                                                         |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `model`   | `anthropic/model-id`; or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`)                                                           |
| `system`  | The system prompt                                                                                                                                            |
| `tools`   | `tool`s and [Restate's tools](#restates-tools); the SDK's (`@beta_async_tool`, which get their input only); and tools the API runs, as dicts (web search)    |
| `client`  | The `AsyncAnthropic` client (a base URL or a gateway of your own); by default one with `ANTHROPIC_API_KEY`                                                   |
| `request` | More of each request, as the API takes it: `{"max_tokens": 4096, "output_config": {"effort": "low"}}`, over `REQUEST` (16000 tokens, `fallbacks: "default"`) |

`model`, `system`, `tools` and `request` may also be functions of the
invocation's context, or async ones (each user's system prompt, from
`ctx.db`, say). Each request is the
API's own JSON, as a durable function's `ctx.call("claude.messages", ...)`
sends it ([host bindings](../../rust/functions/docs/HOST.md)), and with
`fallbacks: "default"` by default: a declined request is retried on another
model by the API itself, and one declined on every model is answered
`DECLINED`. `claude.prompt(ctx, text, ...)`
answers a prompt in a handler of your own, with the same options, once,
however often an attempt crashes (an invocation's first prompt has its id,
the next `<id>:2`, and so on), in the conversation `thread` (by default, the
object's key):

```python
@assistant.handler
async def summarize(ctx, topic: str) -> str:
    return await claude.prompt(ctx, f"Summarize what we said about {topic}")
```

Prompts are numbered in the order they are made, so a handler makes them one
after another: prompts made at once (`asyncio.gather`) may be numbered
differently in a retried attempt, and take each other's answers.

`claude.compact(ctx, instructions)` summarizes the conversation in place of
it, to keep the model's context short, and returns the summary; and
`claude.reset(ctx, handoff)` starts it over, with a note to carry on from,
if given. Each takes effect once per invocation, and refuses while a message
is still being answered
([`examples-python/intermediate/harness.py`](../../examples-python/intermediate/harness.py)):

```python
@assistant.handler(message=True)
async def compact(ctx, instructions: str) -> str:
    return await claude.compact(ctx, instructions)
```

Through crashes: the conversation is the API's messages in the
`restate_claude_messages` table, a row each, in order. Each reply of the model is
stored before its tools run, and their results before the next request; so
a crash during the tools runs them again with the same tool call ids,
without asking the model again. A message's answer is stored with the
model's last reply, and a retried attempt returns it. A tool's
`restate.TerminalError` is its answer to the model (its message); any
other error fails the attempt. The loop is the agent's own, not the SDK's
tool runner (`client.beta.messages.tool_runner`): the runner gives a tool
its input but not its call's id, and cannot start from a stored reply whose
tools did not run.

## Claude Code

`claude_agent.agent(name, model=, system=, tools=, workspace=)` is a virtual
object whose agent is Claude Code itself, on the
[Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-python):
its loop, its tools (read, edit, bash, grep, ... in `workspace`), its
compaction and its skills; and the tools here, served to it in this process
(an SDK MCP server). It has the handlers of the Claude agent; Claude Code's
session is the conversation
([`examples-python/intermediate/coder.py`](../../examples-python/intermediate/coder.py)):

```python
from restate_agents import claude_agent, serve, signal

coder = claude_agent.agent(
    "coder",
    workspace=lambda ctx: f"/work/{ctx.key()}",
    tools=[signal("review", description="Ask a person to review your change, and wait for their verdict.")],
)
serve(coder)
```

| Option           | What                                                                                                                                                                  |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`          | `anthropic/model-id`; or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`)                                                                    |
| `system`         | Added to Claude Code's own system prompt                                                                                                                              |
| `tools`          | `tool`s and [Restate's tools](#restates-tools)                                                                                                                        |
| `workspace`      | A directory of the local machine to work in, with Claude Code's own tools (`Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`); use a sandbox. Without one, only `tools` |
| `claude_options` | More of `ClaudeAgentOptions`: `{"max_turns": 50}`, `{"setting_sources": ["project"]}`, `{"env": {...}}` for the CLI's environment                                     |

Each may be a function of the invocation's context, or an async one (a
workspace for each key, say). `claude_agent.prompt(ctx, text, ...)` answers
a prompt in a handler of your own, once, and `claude_agent.reset(ctx,
handoff)` starts a new session, whose first message carries the handoff.

Each prompt runs the CLI that the SDK bundles, in a process of its own (about
half a second to start), with a configuration directory of its own. Its
transcript is in the object's database (`restate_claude_agent_entries`): the SDK's
`SessionStore`, which the CLI mirrors to as it writes (`eager`), and which a
retried attempt resumes the session from, on any machine. Through crashes:

- **A message is answered once per invocation**: its answer is stored with
  it (`restate_claude_agent_prompts`), and a retried attempt returns it.
- **A turn that a crash cut off is carried on**: the retried attempt resumes
  the session and asks Claude Code to finish the message (the transcript
  may end before the crash: the message is in the request again). Each tool
  call is stored with its input and output (`restate_claude_agent_calls`): a call
  that the model makes again with the same input, in a later attempt of the
  same message, is the earlier call: its id (so Restate's tools, which pass
  it on as their idempotency key, make it once) and, if it finished, its
  output, without running the tool again.
- **A tool's error fails the attempt**, and Restate retries it; a
  `restate.TerminalError` is the tool's answer to the model. An error
  result of Claude Code (an API error) fails the attempt too, unless it is
  a limit of the options (`max_turns`, `max_budget_usd`), which is terminal.
- **`stop`** reaches the turn at its next tool call of the tools here; it
  ends there, answered "Cancelled". **`steer`** reaches the model after the
  next tool call, with its result (a `PostToolUse` hook's context).

Claude Code's own tools run as Claude Code runs them: a crash during an
edit or a command leaves its effects, and the model sees the interruption
when the turn carries on.

## Codex

`codex.agent(name, model=, system=, tools=, workspace=, config=)` is the same
for [Codex](https://github.com/openai/codex), OpenAI's coding agent: its
loop, its tools (shell, apply_patch, ... in `workspace`, in its sandbox) and
its compaction; and the tools here, as its dynamic tools. Each prompt runs
`codex app-server` (the binary of OpenAI's Codex SDK,
`openai-codex-cli-bin`) in a process of its own, speaking its JSON-RPC
protocol over stdio; the conversation is a Codex thread
([`examples-python/intermediate/coder_codex.py`](../../examples-python/intermediate/coder_codex.py)).

| Option      | What                                                                                                                                    |
| ----------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `model`     | `openai/model-id`; or else `MODEL`, if it is OpenAI's; or else Codex's default. With `OPENAI_API_KEY`                                   |
| `system`    | Added to Codex's own instructions (its developer instructions)                                                                          |
| `tools`     | `tool`s and [Restate's tools](#restates-tools)                                                                                          |
| `workspace` | A directory of the local machine, which Codex's tools may write (sandbox `workspace-write`). Without one, an empty one, and `read-only` |
| `config`    | Codex's configuration, as `config.toml` has it: `{"model_reasoning_effort": "high"}`, a model provider of your own                      |

`codex.prompt(ctx, text, ...)` and `codex.reset(ctx, handoff)` are those of
Claude Code. Through crashes, as with Claude Code ([`harness.py`](restate_agents/harness.py)
has what the two share): the thread's rollout (its JSONL transcript) is in
the object's database (`restate_codex_rollouts`), stored before each call of the
tools here and when the turn ends; a retried attempt writes it to a Codex
home of its own and resumes the thread from it (`thread/resume` with its
`path`). A message is answered once per invocation; a turn that a crash cut
off is carried on; a tool call made again with the same input is the
earlier one. `stop` interrupts the turn (`turn/interrupt`) at its next tool
call; `steer` adds to it (`turn/steer`) after the next tool call.

## LiteLLM

`litellm.agent(name, model=, system=, tools=)` is the same agent on any model,
through [LiteLLM](https://docs.litellm.ai): the OpenAI-format messages that
LiteLLM speaks to every provider, stored in the object's database
(`restate_litellm_messages`), with the handlers of the Claude agent. As with Claude,
the loop is the agent's own: each reply of the model is stored before its
tools run, and their results before the next request.

```python
from restate_agents import litellm, serve

assistant = litellm.agent("assistant", model="openai/gpt-5", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

| Option    | What                                                                                                                                                                                                             |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`   | Any model LiteLLM knows, as `provider/model-id` (`openai/gpt-5`, `gemini/gemini-2.5-pro`, `bedrock/...`, `ollama/llama3.1`), with its provider's API key; or else `MODEL`, or else `anthropic/claude-sonnet-5-5` |
| `system`  | The system prompt                                                                                                                                                                                                |
| `tools`   | `tool`s and [Restate's tools](#restates-tools); and tools the provider runs, as dicts in its format                                                                                                              |
| `request` | More of each request, as `litellm.acompletion` takes it: `{"temperature": 0.2, "api_base": "http://localhost:11434"}`                                                                                            |

`model`, `system`, `tools` and `request` may be functions of the
invocation's context, or async ones. A model LiteLLM knows no provider of
fails when the agent is defined. `litellm.prompt(ctx, text, ...)`,
`litellm.compact(ctx, instructions)` and `litellm.reset(ctx, handoff)` are
those of Claude ([`examples-python/intermediate/release_notes.py`](../../examples-python/intermediate/release_notes.py): two models, one drafting and one reviewing).

## LangGraph

`langgraph.agent(name, model=, system=, tools=)` is a virtual object, one per
key (its thread), with the handlers `chat` (a message in, the answer out; as
plain text, `-d 'Hi'`, or a JSON string, `--json '"Hi"'`) and `messages` (the
conversation so far, with the tool calls, a page at a time:
`{"limit": 50, "before": id}`, or what is new since a cursor: `{"cursor": n}`); `status`, `stop` and `steer`
([Turns](#turns)); `pending` and `signal` ([Restate's tools](#restates-tools));
and `sync`, `notifications` and `watch` ([Follow a conversation](#follow-a-conversation)).
Add handlers of your own with `@assistant.handler`.

| Option   | What                                                                                                                                                                                                                        |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `anthropic/model-id` (`ChatAnthropic`); or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`); or any LangChain chat model that calls tools; or a function of the invocation's context giving either |
| `system` | The system prompt                                                                                                                                                                                                           |
| `tools`  | `tool`s, [Restate's tools](#restates-tools) and LangChain tools, or functions of the invocation's context giving them                                                                                                       |

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
`model.bind_tools(langgraph.tools(tools))`)
([`examples-python/intermediate/triage.py`](../../examples-python/intermediate/triage.py),
[`examples-python/intermediate/writer.py`](../../examples-python/intermediate/writer.py)):

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
`signal` tool, or a `signal` node.

### Restate's steps in a graph

Restate's tools are for a model to choose; in a graph of your own, the
graph can take Restate's steps itself, as nodes
(`restate_agents.langgraph.nodes`): `call(handler)`, `send(handler)`,
`workflow(target)` and `signal(name)`, each made once per run of its node,
however often an attempt crashes (its idempotency key is the invocation's
id and LangGraph's task id, which a step that a crash cut off keeps)
([`examples-python/intermediate/writer.py`](../../examples-python/intermediate/writer.py): a person approves
each post, and a workflow publishes it):

```python
from restate_agents.langgraph import nodes

graph.add_node("status", nodes.call(status, input=lambda state: state["order"], update="status"))
graph.add_node("approval", nodes.signal("approval", input=lambda state: {"draft": state["draft"]}, update="verdict"))
graph.add_node("publish", nodes.workflow(publishing, input=lambda state: state["draft"], update="url"))
graph.add_node("follow_up", nodes.send(checkin, input=lambda state: state["order"], delay=timedelta(days=1)))
```

- **`input`**: a function of the state giving the handler's input (or, for
  `signal`, what it waits for, which `pending` shows).
- **`update`**: where the result goes: a key of the state, or a function of
  the state and the result giving the update. The result is the handler's,
  as its output type says (a pydantic model, say), not text.
- **`key`**: an object's or a workflow's key, or a function of the state
  giving it. A `workflow` node runs a workflow of its own per run, keyed
  `<this key>-<task id>`, unless given one.

A handler's `restate.TerminalError` fails the turn: no model reads it. A
stopped turn ends at a node that waits, as at a tool that does.

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

`pydantic_ai.agent(name, model=, system=, tools=)` is the same virtual
object (`chat`, `messages`, `status`, `stop`, `steer`, `pending`, `signal`)
on [Pydantic AI](https://ai.pydantic.dev):

```python
from restate_agents import pydantic_ai, serve

assistant = pydantic_ai.agent("assistant", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

| Option   | What                                                                                                                                                                                                                   |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `provider/model-id` (any of Pydantic AI's providers whose package is installed: `anthropic` is), a Pydantic AI model, or a function of the context giving one. Default: `MODEL`, or else `anthropic/claude-sonnet-5-5` |
| `system` | The system prompt (Pydantic AI's instructions)                                                                                                                                                                         |
| `tools`  | `tool`s, Restate's tools, and Pydantic AI's tools or tool functions (`RunContext` and all)                                                                                                                             |

Or bring an `Agent` of your own, whose tools include Restate's through
`pydantic_ai.tools([...])`; and ask it from handlers of your own with `pydantic_ai.prompt(ctx,
agent, text, **run)` (`run`: `Agent.run`'s options, such as `output_type` or
`deps`), each prompt answered once
([`examples-python/intermediate/trips.py`](../../examples-python/intermediate/trips.py)):

```python
from pydantic_ai import Agent
from restate_agents import pydantic_ai

planner = Agent("anthropic:claude-sonnet-5-5", tools=[weather, *pydantic_ai.tools([signal("approval", ...)])])
trips = pydantic_ai.agent("trips", planner)


@trips.handler
async def plan(ctx) -> Plan:
    return await pydantic_ai.prompt(ctx, planner, "Write down the trip we planned, day by day.", output_type=Plan)
```

Through crashes, as with LangGraph: the conversation (a row per message, in
`restate_pydantic_ai_messages`) is stored before each model request and with each
response, so a retried attempt carries on from there: the tool calls a model
made run again with the same ids, without asking the model again; the
message it answers is in it, marked with the invocation's id, and a retried
attempt that finds it answered returns the answer (a structured output too)
without asking the model again. A tool's `restate.TerminalError` is its
answer to the model; any other error fails the attempt.

## OpenAI Agents SDK

`openai_agents.agent(name, model=, system=, tools=)` is the same virtual
object (`chat`, `messages`, `status`, `stop`, `steer`, `pending`, `signal`)
on the [OpenAI Agents SDK](https://openai.github.io/openai-agents-python/)
([`examples-python/intermediate/airline.py`](../../examples-python/intermediate/airline.py): a handoff, and Restate's tools):

```python
from restate_agents import openai_agents, serve

assistant = openai_agents.agent("assistant", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

| Option   | What                                                                                                                                                             |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `openai/model-id`, a model of the SDK, or a function of the context giving one; or else `MODEL`, if it is OpenAI's; or else the SDK's default (`OPENAI_API_KEY`) |
| `system` | The system prompt (the Agent's instructions)                                                                                                                     |
| `tools`  | `tool`s, Restate's tools, and the SDK's tools (`@function_tool`, hosted tools)                                                                                   |

Or bring an `Agent` of your own (handoffs, guardrails, an output type),
whose tools include Restate's through `openai_agents.tools([...])`; and ask it
from handlers of your own with `openai_agents.prompt(ctx, agent, text,
**run)` (`run`: `Runner.run`'s options, such as `context`, `max_turns` or
`run_config`), each prompt answered once, its typed output too.

A handoff holds: the conversation's next message goes to the agent it was
handed off to (kept in `restate_openai_agents_current`).

Through crashes, as with Pydantic AI: the conversation (an item per row, in
`restate_openai_agents_items`) is stored before each model request (the run's
`call_model_input_filter`) and with each response (its `on_llm_end` hook),
so the model's tool calls are stored before any tool runs. A retried attempt
runs the stored calls again, with their ids (the tool's `on_invoke_tool`),
without asking the model again, then carries on; a stored reply whose calls
cannot be run again (a handoff, a hosted tool's) is dropped, and the agent
that made it is asked again. Each prompt's answer is
stored with it (`restate_openai_agents_prompts`). A tool's `restate.TerminalError` is
its answer to the model; any other error fails the attempt (the SDK's
`@function_tool`s answer their errors to the model, as they do anywhere).
A message steered to the turn joins it after its next tool call. The run
takes no `hooks` of yours: give an `Agent` its own (`Agent.hooks`).

## Google ADK

`adk.agent(name, model=, system=, tools=)` is the same virtual object
on Google's [Agent Development Kit](https://google.github.io/adk-docs/)
([`examples-python/intermediate/oncall.py`](../../examples-python/intermediate/oncall.py): an incident responder on Restate's tools):

```python
from restate_agents import adk, serve

assistant = adk.agent("assistant", model="google/gemini-2.5-flash", system="Answer briefly.", tools=[get_weather])
serve(assistant)
```

| Option   | What                                                                                                                                                   |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `model`  | `google/model-id`, an ADK model, or a function of the context giving one; or else `MODEL`, if it is Google's; or else ADK's default (`GOOGLE_API_KEY`) |
| `system` | The system prompt (ADK's instruction)                                                                                                                  |
| `tools`  | `tool`s, Restate's tools, and ADK's tools (functions, `FunctionTool`s, ...)                                                                            |

Or bring an agent of your own (sub-agents, callbacks, an output schema), or
an `App` (its plugins), whose tools include Restate's through
`adk.tools([...])`; and ask it from handlers of your own with
`adk.prompt(ctx, agent, text, **run)` (`run`: `Runner.run_async`'s options),
each prompt answered once.

The conversation is an ADK session whose id is the object's key: a session
service of the object's database (`restate_adk_events`, an event per row; its state
is that of its events' state changes), which the runner appends each event
to as it goes, so the model's tool calls are stored before they run. A
retried attempt runs the stored calls of the tools here again, with their
ids, without asking the model again, then carries on (`run_async` without a
new message); for a call of ADK's own tools, ADK asks the model again, as
its own resumption does. A tool's `restate.TerminalError` is its answer to
the model; any other error fails the attempt (its error event is not
stored). A message steered to the turn joins it before the model's next
request after a tool call (a plugin's `before_model_callback`), as a user
event of the session.

## Tools

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

Most frameworks take their own tools too: see each one's `tools`.

## Restate's tools

Tools that call, send to, start and wait for what Restate runs, each once
per tool call, however often an attempt crashes (its idempotency key is the
tool call's), as in [`examples-python/intermediate/support.py`](../../examples-python/intermediate/support.py):

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

## Sandboxes

`restate_agents.sandbox` gives an agent tools that run in a sandbox, while
its loop runs where it does: `list_files`, `read_file`, `write_file` and
`run_command`, in a workspace of the conversation's own, whose files last
from one message to the next
([`examples-python/intermediate/analyst.py`](../../examples-python/intermediate/analyst.py)):

```python
from restate_agents import claude, sandbox

box = sandbox.modal(image="python:3.13-slim")
analyst = claude.agent("analyst", system="...", tools=box.tools())
```

| Provider                                     | Where                                                                                                                                                                                                                    |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `sandbox.modal(image=, app=, idle_timeout=)` | A [Modal Sandbox](https://modal.com/docs/guide/sandbox) per conversation, its files in a Volume of its own; its compute stops once idle, and starts again on the next tool call (`MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`) |
| `sandbox.local(root)`                        | A directory of this machine, commands in it with a bare environment (no API keys): for development, it isolates nothing                                                                                                  |

The first sandbox tool of an attempt opens the conversation's sandbox: it
makes it, or resumes the one it made before, whose reference is in the
object's database (`restate_sandboxes`). Its name is the conversation's, so an
attempt that retries after it was made finds it rather than making another;
parallel tool calls share it. A command that a crash cut off runs again
(the integrations run an unfinished tool call again), so a command that must
not run twice makes itself safe to. `box.destroy(ctx)`, in a handler of your
own, removes the sandbox and its files. Another provider implements
`open(name, ref)` and `destroy(ref)` (`sandbox.Provider`), and
`sandbox.Sandbox(provider)` gives its tools.

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
- **`steer`** adds a message to the turn: the model reads it with the
  results of its next tool call (in Pydantic AI and ADK, before its next
  request; in Codex, as Codex steers a turn).
  A turn that ends before (no tool call came) answers it as a message of its
  own; with no turn, `steer` is a message of its own (`"steered": false`, and
  its invocation, to attach to). A stopped turn answers none of them. The
  steered messages wait in the agent's `signals` object, and go into the
  thread by their ids: a retried attempt neither loses nor repeats one.

## Follow a conversation

The conversation as clients read it is a log in the object's database: each
change the agent stores goes into it. Each message has an `id`, which never
changes, and the log has a cursor, which each change advances: a message
that grows (a tool call with its output) comes again, with its `id`, and a
compacted or reset conversation keeps what was said, the summary or the
handoff after it (the model's context is what changed). `messages` reads it:
a page of the latest (`{"limit": 50}`), or older ones (`{"before": 12}`), each
with the cursor to follow on from; or what is new or changed since a cursor
(`{"cursor": 31}`), by when it changed, with the next cursor:

```sh
curl localhost:8080/support/alice/messages --json '{"limit": 50}'    # {"messages": [...], "more": false, "cursor": 31}
curl localhost:8080/support/alice/messages --json '{"cursor": 31}'   # what is new or changed since, and the next cursor
```

`sync` waits for it: with the `revision` of the last sync, it answers once
something changed (a message, a wait, a turn's start or end), or at its
timeout (50 s by default, at most 60), with the new revision, what the agent
is doing (`status`), and the messages since the `cursor`, in one request. It
waits in Restate, holding no process; sent again with the same idempotency
key, it joins the same wait. (`notifications` and `watch({"revision"})`
are the revision alone, and the wait alone.)

```sh
curl localhost:8080/support/alice/sync --json '{}'                                 # {"revision": 7, "cursor": 31, "status": ..., "messages": [...], "more": false}
curl localhost:8080/support/alice/sync --json '{"cursor": 31, "revision": 7}'      # the same, once something changed
```

`restate_agents.client` does the reading and the waiting, from any Python
code, with nothing of the agent's: `AgentClient(url, agent, key)` sends
messages (`chat`, `send`), reads (`messages`, `status`, `pending`, `sync`),
approves, steers and stops; and `follow()` gives the conversation as it
changes: what the agent is doing, and the messages new or changed since the
last time (keep them by `id`), and the cursor to carry on from later
(`follow(cursor=...)`). A network failure does not end it, and a sync it
retries joins the one already waiting in Restate
([`examples-python/basic/follow.py`](../../examples-python/basic/follow.py)):

```python
from restate_agents.client import AgentClient, as_text

async with AgentClient("http://localhost:8080", "support", "alice") as alice:
    await alice.send("Order 103 arrived broken, can I get my $30 back?")
    async for change in alice.follow():
        for message in change.messages:
            print(as_text(message))
        for wait in change.waiting:
            await alice.signal("approved", id=wait["id"])
```

`as_text(message)` is a message to read, its tool calls with their outputs;
`change.waiting` is what the agent waits for (`change.status["waiting"]`).

The handlers are those of the TypeScript agents, so the chat page of
[`examples-typescript/ui`](../../examples-typescript/ui/app.ts) serves the
Python `support.py` as it is: its conversation as the agent works, Approve
and Reject, Steer and Stop.

## Handlers of your own

An agent is a virtual object with a SQL database of its own, `ctx.db`, one
per key: `restate_fns.VirtualObject`, retries for weeks (50 ms after a
crash, then twice as long each time, up to 10 s apart), since each attempt
resumes from the database, and the SQL storage hidden from the journal in
Restate's UI.
`@assistant.handler` adds a handler, which takes `ctx`, then its input: one
parameter, or several, which a JSON object gives by name
([`examples-python/basic/memory.py`](../../examples-python/basic/memory.py)). With
`@assistant.handler(message=True)`, its input is a message, as `chat`'s is:
plain text or a JSON string (`curl -d 'Keep it short'`), or nothing. A
shared handler (`@assistant.handler(kind="shared")`) runs beside the message
being answered, and reads `ctx.db` as stored: it writes nothing. A handler
without input also takes an empty request, or one with a JSON body, as
TypeScript's do, so any Restate client calls it. `serve(*services)`
serves them on `PORT`, or 9080; with `background=True`, on a thread of its
own, returning at once, as in a Modal Server
([`examples-python/intermediate/on_modal.py`](../../examples-python/intermediate/on_modal.py)).

An object of your own, without an agent of its own, is an `Object`: the same
virtual object, with `ctx.db`, its handlers yours. `@obj.chat` makes its
`chat` handler, which takes its message as an agent's does (plain text, or a
JSON string). `ask(ctx, agent, message)` sends a message to an agent's
`chat` (keyed `key`, by default this object's key) and returns the answer:
once per invocation, however often an attempt runs, as its call has an
idempotency key (the invocation's id, the agent's name and how many messages
the attempt sent before, unless given one). Here, a front desk asks a model
which team handles a message, then hands it to that team
([`examples-python/intermediate/router.py`](../../examples-python/intermediate/router.py)):

```python
from restate_agents import Object, ask, claude

desk = Object("desk")


@desk.chat
async def chat(ctx, message: str) -> str:
    team = await claude.prompt(ctx, f"Which team handles this message, billing or tech? One word.\n\n{message}")
    return await ask(ctx, billing if "billing" in team.lower() else tech, message)
```

## Tests

`make test-py`, from the repository's root:

- [`tests/test_checkpointer.py`](tests/test_checkpointer.py): LangGraph's
  checkpointer conformance suite (`langgraph-checkpoint-conformance`, its 58
  base tests), over the extension and an in-memory store.
- [`tests/test_langgraph.py`](tests/test_langgraph.py): the agent on a
  scripted model (the teller, [`tests/fakes.py`](tests/fakes.py)), over a fake
  object state: the thread, an answer once per invocation, a crash in a tool,
  a crash at each state request of a message (with `durability="async"`, it
  moves money twice), an interrupted run, the system prompt, and tools'
  errors.
- [`tests/test_claude.py`](tests/test_claude.py): the same, for the Claude
  agent (the teller as an Anthropic client), with options for each
  invocation, the SDK's tools, and prompts of your own, each answered once.
- [`tests/test_claude_agent.py`](tests/test_claude_agent.py): the same,
  for Claude Code: the CLI the SDK bundles, calling the teller as the
  Messages API ([`tests/fakes.py`](tests/fakes.py), `MessagesServer`), with
  crashes at each state request of a message, steering, stop and reset.
- [`tests/test_codex.py`](tests/test_codex.py): the same, for Codex:
  `codex app-server`, calling the teller as the Responses API
  ([`tests/fakes.py`](tests/fakes.py), `ResponsesServer`, a model provider
  of Codex's configuration).
- [`tests/test_openai_agents.py`](tests/test_openai_agents.py): the same,
  for the OpenAI Agents SDK (the teller as an SDK model), with an `Agent`
  of your own and its typed output, each answered once, and a handoff that
  holds for the next messages, with a crash at each of its state requests.
- [`tests/test_adk.py`](tests/test_adk.py): the same, for Google ADK (the
  teller as an ADK model), with an agent of your own in a thread of its own.
- [`tests/test_litellm.py`](tests/test_litellm.py): the same, for the
  LiteLLM agent (the teller as a LiteLLM provider, `teller/...`), with
  prompts, compacting and resetting of your own.
- [`tests/test_pydantic_ai.py`](tests/test_pydantic_ai.py): the same, for
  the Pydantic AI agent (the teller as a Pydantic AI model), and agents of
  your own, prompts of your own and a structured output, each answered once.
- [`tests/test_harness.py`](tests/test_harness.py): the answer-once loop of
  Claude Code, Codex, the OpenAI Agents SDK and ADK: a turn that finishes a
  cut-off message takes the messages steered into it.
- [`tests/test_client.py`](tests/test_client.py): a message as text, as
  `as_text` gives it.
- [`tests/test_sandbox.py`](tests/test_sandbox.py): the sandbox's tools on
  the local provider: files from one message to the next, commands without
  the process's environment, a timeout, paths kept in the workspace, one
  opening for parallel calls, and an agent that writes and runs code.
- [`tests/test_log.py`](tests/test_log.py): the conversation as clients
  read it: ids that never change, a cursor of what is new or changed (a tool
  call's output), pages back, and a compaction that keeps what was said.
- [`tests/test_object.py`](tests/test_object.py): a `chat` handler of your
  own, and `ask`, its idempotency keys through a retried attempt.
- [`tests/test_tools.py`](tests/test_tools.py): Restate's tools on a fake
  context: their names, the model's input, and the calls they make.

Against a real Restate server ([`../tests`](../tests)): `make
concurrency-test-py` has the agent moving money through Restate calls, and
Restate's tools on each agent
([`test_restate_tools.py`](../tests/test_restate_tools.py): a call, a signal
and a workflow in one turn, signals kept and dropped, a cancelled wait, a
subagent, a send, status, stop and steer), and
`make crash-test-py` prompts it while the process crashes, checking that each
prompt moved money exactly once.

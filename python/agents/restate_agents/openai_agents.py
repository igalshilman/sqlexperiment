"""
OpenAI Agents SDK agents in Restate virtual objects: each conversation is in
the object's database, stored before each model request and with each
response::

    from restate_agents import serve, tool
    from restate_agents.openai_agents import agent

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = agent("assistant", instructions="Answer briefly.", tools=[get_weather])
    serve(assistant)

The usual agent is an ``Agent`` with ``model`` (``openai/model-id``, or a
model of the SDK), or else MODEL if it is OpenAI's, or else the SDK's
default, with OPENAI_API_KEY; ``instructions``; and ``tools``. Or bring an
``Agent`` of your own (handoffs, guardrails, output types and all), whose
tools may include those here through ``function_tools([...])``.

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and the run carries on from the stored items:

- The model's reply is stored before its tools run, and their results
  before the next request. A crash during the tools runs the stored calls
  again, with the same call ids, without asking the model again; a tool
  passes its ``call_id`` on to stay idempotent.
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A message is answered once per invocation: its answer is stored with it,
  and a retried attempt returns it.
- A cancelled turn (``stop``) ends: each of its cut-off tool calls, and the
  turn, are answered "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) joins it after the next tool
  call, and the model reads it with the results.
"""

from __future__ import annotations

import dataclasses
import inspect
import json
import os
import uuid
from collections.abc import Callable, Sequence
from typing import Any

import restate_fns as restate
from agents import Agent, FunctionTool, Model, RunConfig, RunContextWrapper, RunHooks, Runner, UserError
from agents.run_config import CallModelData, ModelInputData
from agents.tool_context import ToolContext
from pydantic import TypeAdapter, ValidationError
from pydantic_core import to_json
from restate_fns import ObjectContext

from restate_agents import harness, turns
from restate_agents.common import Transcript, has_table, model_id_of
from restate_agents.object import Object
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "agent", "function_tools", "prompt"]

# The tables of the conversations' messages and answers, of their items, and of the agent each is with.
PROMPTS, ITEMS, CURRENT = "openai_agents_prompts", "openai_agents_items", "openai_agents_current"


def agent(
    name: str,
    openai_agent: Agent[Any] | Callable[[ObjectContext], Agent[Any]] | None = None,
    *,
    model: str | Model | Callable[[ObjectContext], str | Model] | None = None,
    instructions: str | None = None,
    tools: Sequence[Tool | Any] = (),
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    The usual agent takes ``model``: ``openai/model-id``, a model of the SDK,
    or a function of the invocation's context giving either;
    ``instructions``; and ``tools``: ``tool``s, Restate's tools, and the
    SDK's tools. Or ``openai_agent``: an ``Agent`` of your own, or a function
    of the context giving one.

    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if openai_agent is not None and (model is not None or instructions is not None or tools):
        raise TypeError(
            f"{name}: an agent takes an Agent, or the usual agent's model, instructions and tools; not both"
        )
    if not callable(model):
        _model_of(model)
    usual = Agent(name=name, instructions=instructions, tools=function_tools(tools)) if openai_agent is None else None

    async def answer(ctx: ObjectContext, message: str) -> str:
        if usual is not None:
            chosen = model(ctx) if callable(model) and not isinstance(model, Model) else model
            given = _model_of(chosen)
            return await prompt(ctx, usual, message, **({"run_config": RunConfig(model=given)} if given else {}))
        chosen = openai_agent if isinstance(openai_agent, Agent) else openai_agent(ctx)
        return text_of(await prompt(ctx, chosen, message))

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        if not await has_table(ctx.db, ITEMS):
            return transcript
        for _, item in await _Items(ctx.db, ctx.key()).load():
            _transcribe(transcript, item)
        return transcript

    return turns.agent(name, answer, transcript, **options)


def function_tools(given: Sequence[Tool | Any]) -> list[Any]:
    """
    ``given`` as the SDK's tools, for an ``Agent`` of your own::

        Agent(name="support", tools=function_tools([call(status), signal("approval", ...)]))

    ``given``: ``tool``s, Restate's tools, and the SDK's tools, as they are.
    """
    return [_function_tool(t) if isinstance(t, Tool) else t for t in given]


async def prompt(ctx: Any, agent: Agent[Any], text: str, *, thread: str | None = None, **run: Any) -> Any:
    """
    Answer ``text`` with ``agent``, in the conversation ``thread`` (by
    default, the object's key), and return its final output. In a handler of
    your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await prompt(ctx, summarizer, f"Summarize what we said about {topic}")

    ``run``: the options of ``Runner.run``, such as ``context``,
    ``max_turns`` or ``run_config``. Each prompt is answered once, however
    often an attempt crashes: the invocation's first prompt has its id, the
    next ``<id>:2``, and so on.
    """
    if "hooks" in run:
        raise TypeError("prompt runs the agent with hooks of its own: give hooks to the Agent (Agent.hooks)")
    thread = thread or ctx.key()
    prompts, items = harness.Prompts(ctx.db, PROMPTS, thread), _Items(ctx.db, thread)

    async def answer(pending: harness.Prompt, _: str) -> str:
        output = await _run(ctx, agent, pending, items, run)
        return output if isinstance(output, str) else to_json(output).decode()

    async def cancelled(pending: harness.Prompt) -> None:
        await _cancel(items)

    async def steered() -> list[str]:
        return _steered(await items.load())

    return _output(agent, await harness.answer_once(ctx, prompts, text, answer, cancelled=cancelled, steered=steered))


class _Items:
    """
    A conversation's items in the object's database, a row each, in order, each
    with its tag: the prompt that it asks (``prompt:<id>``), the model's
    response that it is part of (``response:<id>``), or the steered message
    that it is (``steer:<id>``).
    """

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self._saved: list[str] = []

    async def load(self) -> list[tuple[str | None, dict[str, Any]]]:
        if not await has_table(self.db, ITEMS):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {ITEMS} (
                  thread TEXT NOT NULL,
                  n INTEGER NOT NULL,
                  tag TEXT,
                  item TEXT NOT NULL,
                  PRIMARY KEY (thread, n)
                )
            """)
        rows = await self.db.query(f"SELECT tag, item FROM {ITEMS} WHERE thread = ? ORDER BY n", [self.thread])
        self._saved = [item for _, item in rows]
        return [(tag, json.loads(item)) for tag, item in rows]

    async def save(self, items: Sequence[dict[str, Any]], tags: dict[int, str] | None = None) -> None:
        """Store ``items``, the conversation as it is now: those not stored yet, with their ``tags`` (by position)."""
        dumped = [json.dumps(item) for item in items]
        first = next((i for i, (a, b) in enumerate(zip(dumped, self._saved, strict=False)) if a != b), None)
        first = min(len(dumped), len(self._saved)) if first is None else first
        if first == len(dumped) == len(self._saved):
            return
        async with self.db.transaction() as tx:
            await tx.execute(f"DELETE FROM {ITEMS} WHERE thread = ? AND n >= ?", [self.thread, first])
            for n in range(first, len(dumped)):
                tag = (tags or {}).get(n)
                await tx.execute(f"INSERT INTO {ITEMS} VALUES (?, ?, ?, ?)", [self.thread, n, tag, dumped[n]])
        self._saved = dumped
        await turns.publish()


async def _run(ctx: Any, agent: Agent[Any], pending: harness.Prompt, items: _Items, run: dict[str, Any]) -> Any:
    """Run ``agent`` on from the stored items to the answer of the prompt ``pending``."""
    stored = await items.load()
    tags = {n: tag for n, (tag, _) in enumerate(stored) if tag}
    history = [item for _, item in stored]
    turns.taken(_steered(stored))
    asked = f"prompt:{pending.prompt}"
    if asked not in tags.values():
        tags[len(history)] = asked
        history.append({"role": "user", "content": pending.text})
        await items.save(history, tags)
    elif history[-1].get("type") == "message" and history[-1].get("role") == "assistant":
        # The model answered, and a crash came before the answer was stored.
        return _output(agent, _text(history[-1]))
    context = run.get("context")
    # The agent the conversation is with: the root, or the one a handoff gave it to.
    current = _Current(items.db, items.thread)
    everyone = _agents(agent)
    agent = everyone.get(await current.get() or agent.name, agent)
    if unanswered := _unanswered(history):
        found = {t.name: t for t in await agent.get_all_tools(RunContextWrapper(context))}
        if all(isinstance(found.get(c["name"]), FunctionTool) for c in unanswered):
            # The model's calls, stored before a crash cut their tools off: run them again, with their ids.
            for call in unanswered:
                tool = found[call["name"]]
                given = ToolContext(
                    context, tool_name=call["name"], tool_call_id=call["call_id"], tool_arguments=call["arguments"]
                )
                output = await _unwrap(tool.on_invoke_tool(given, call["arguments"]))
                history.append({"type": "function_call_output", "call_id": call["call_id"], "output": text_of(output)})
        else:
            # A reply that cannot be carried on (a handoff, a hosted tool's call): its agent is asked again.
            cut = tags.get(history.index(unanswered[0]))
            if cut is not None:
                agent = everyone.get(cut.split(":", 2)[2], agent)
            kept = [n for n in range(len(history)) if cut is None or tags.get(n) != cut]
            history = [history[n] for n in kept]
            tags = {i: tags[n] for i, n in enumerate(kept) if n in tags}
        await items.save(history, tags)
    seen: list[dict[str, Any]] = list(history)

    async def before(data: CallModelData[Any]) -> ModelInputData:
        nonlocal seen
        given = data.model_data
        if user_filter is not None:  # the run_config's own filter, first
            given = user_filter(data)
            given = await given if inspect.isawaitable(given) else given
        input = list(given.input)
        if input and input[-1].get("type") == "function_call_output":
            for steered in await turns.steering(ctx):
                tags[len(input)] = f"steer:{steered.id}"
                input.append({"role": "user", "content": harness.STEERED + steered.text})
        await items.save(input, tags)
        turns.taken(_steered([(tags.get(n), i) for n, i in enumerate(input)]))
        seen = input
        return ModelInputData(input=input, instructions=given.instructions)

    class Hooks(RunHooks[Any]):
        async def on_agent_start(self, context: Any, agent: Any) -> None:
            await current.set(agent.name)

        async def on_llm_end(self, context: Any, agent: Any, response: Any) -> None:
            nonlocal seen
            reply = list(response.to_input_items())
            tag = f"response:{uuid.uuid4().hex}:{agent.name}"
            tags.update({n: tag for n in range(len(seen), len(seen) + len(reply))})
            seen = [*seen, *reply]
            await items.save(seen, tags)

    run = dict(run)
    config = run.pop("run_config", None) or RunConfig()
    user_filter = config.call_model_input_filter
    config = dataclasses.replace(config, call_model_input_filter=before)
    result = await _unwrap(Runner.run(agent, history, hooks=Hooks(), run_config=config, **run))
    return result.final_output


class _Current:
    """The agent a conversation is with, by name, in the object's database: none, for the root."""

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self.name: str | None = None

    async def get(self) -> str | None:
        if not await has_table(self.db, CURRENT):
            await self.db.execute(
                f"CREATE TABLE IF NOT EXISTS {CURRENT} (thread TEXT PRIMARY KEY, agent TEXT NOT NULL)"
            )
        rows = await self.db.query(f"SELECT agent FROM {CURRENT} WHERE thread = ?", [self.thread])
        self.name = rows[0][0] if rows else None
        return self.name

    async def set(self, name: str) -> None:
        if name != self.name:
            await self.db.execute(f"INSERT OR REPLACE INTO {CURRENT} VALUES (?, ?)", [self.thread, name])
            self.name = name


def _agents(root: Agent[Any]) -> dict[str, Agent[Any]]:
    """The agents that ``root`` and its handoffs reach, by name."""
    found: dict[str, Agent[Any]] = {}
    waiting = [root]
    while waiting:
        agent = waiting.pop()
        if agent.name in found:
            continue
        found[agent.name] = agent
        for handoff in agent.handoffs:
            target = handoff if isinstance(handoff, Agent) else getattr(handoff, "_agent_ref", lambda: None)()
            if isinstance(target, Agent):
                waiting.append(target)
    return found


async def _unwrap(work: Any) -> Any:
    """``work``'s result: a tool's error as it was raised, not as the SDK wraps it (a ``UserError``)."""
    try:
        return await work
    except UserError as error:
        if isinstance(error.__cause__, Exception):
            raise error.__cause__ from None
        raise


async def _cancel(items: _Items) -> None:
    """End a cancelled run: a result for each tool call without one, then ``CANCELLED``."""
    stored = await items.load()
    tags = {n: tag for n, (tag, _) in enumerate(stored) if tag}
    history = [item for _, item in stored]
    history += [
        {"type": "function_call_output", "call_id": c["call_id"], "output": CANCELLED} for c in _unanswered(history)
    ]
    history.append({"role": "assistant", "content": CANCELLED})
    await items.save(history, tags)


def _unanswered(history: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The function calls of ``history`` without an output."""
    answered = {i["call_id"] for i in history if i.get("type") == "function_call_output"}
    return [i for i in history if i.get("type") == "function_call" and i["call_id"] not in answered]


def _steered(stored: Sequence[tuple[str | None, dict[str, Any]]]) -> list[str]:
    """The ids of the steered messages in ``stored``."""
    return [tag.removeprefix("steer:") for tag, _ in stored if tag and tag.startswith("steer:")]


def _output(agent: Agent[Any], answer: str) -> Any:
    """The final output of ``agent``, from its stored ``answer``: the text, or its output type from the JSON."""
    if agent.output_type in (None, str):
        return answer
    return TypeAdapter(agent.output_type).validate_json(answer)


def _text(item: dict[str, Any]) -> str:
    """The text of a message item."""
    content = item.get("content")
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content or [] if "text" in part)


def _model_of(model: str | Model | None) -> str | Model | None:
    """The model of a run: a model of the SDK, or ``openai/model-id``'s id, or MODEL's if it is OpenAI's."""
    if isinstance(model, Model):
        return model
    if model is None:
        named = os.environ.get("MODEL", "")
        model = named if named.startswith("openai/") else None
    return model_id_of(model, "openai") if model is not None else None


def _function_tool(tool: Tool) -> FunctionTool:
    """``tool`` as the SDK's function tool, which gets its call's id."""

    async def invoke(context: ToolContext[Any], arguments: str) -> str:
        try:
            return text_of(await tool.invoke(json.loads(arguments or "{}"), context.tool_call_id))
        except (ValidationError, ValueError) as error:
            return f"Error: {error}"
        except restate.TerminalError as error:
            if error.status_code == 409:
                raise
            return error.message

    return FunctionTool(
        name=tool.name,
        description=tool.description,
        params_json_schema=tool.input.model_json_schema(),
        on_invoke_tool=invoke,
        strict_json_schema=False,
    )


def _transcribe(transcript: Transcript, item: dict[str, Any]) -> None:
    """An item, as ``messages`` gives it."""
    kind = item.get("type", "message")
    if kind == "message" and item.get("role") in ("user", "assistant"):
        transcript.say(item["role"], _text(item).removeprefix(harness.STEERED))
    elif kind == "function_call":
        try:
            input = json.loads(item.get("arguments") or "{}")
        except ValueError:
            input = item.get("arguments")
        transcript.call(item["call_id"], item["name"], input)
    elif kind == "function_call_output":
        output = item.get("output")
        transcript.result(item["call_id"], output if isinstance(output, str) else json.dumps(output))

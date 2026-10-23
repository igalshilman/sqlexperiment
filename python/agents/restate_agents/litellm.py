"""
Agents on any model, through LiteLLM, in Restate virtual objects: the
OpenAI-format messages that LiteLLM speaks to every provider, stored in the
object's database::

    from restate_agents import litellm, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = litellm.agent("assistant", model="openai/gpt-5", system="Answer briefly.", tools=[get_weather])
    serve(assistant)

The model is ``model``, as LiteLLM names it (``provider/model-id``: such as
``anthropic/claude-sonnet-5-5``, ``openai/gpt-5``, ``gemini/gemini-2.5-pro``,
``ollama/llama3.1``), or else MODEL, or else anthropic/claude-sonnet-5-5, with
its provider's API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, ...). Each request
is ``litellm.acompletion``'s, with more of it from ``request``.

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, with the stored messages:

- Each reply of the model is stored before its tools run, and their results
  before the next request. So a crash during the tools runs them again with
  the same tool call ids, without asking the model again, and a tool passes
  its ``call_id`` on to stay idempotent.
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A message is answered once per invocation: its answer is stored with the
  model's last reply, and a retried attempt returns it. A message that a
  crash cut off is finished before the next one starts.
- A cancelled turn (``stop``) ends: each of its cut-off tool calls, and the
  turn, are answered "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) follows the tools' results, and the
  model reads it with them.
"""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import Any

import litellm
import restate_fns as restate
from pydantic import ValidationError
from restate_fns import ObjectContext

from restate_agents import conversation, turns
from restate_agents.common import DEFAULT_MODEL, ModelError, Transcript, configured, model_of
from restate_agents.conversation import Conversation, answer_of, idle, pending_of
from restate_agents.object import Object
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DEFAULT_MODEL", "agent", "compact", "prompt", "reset"]

# A tool: a ``tool`` or one of Restate's; or a tool the provider runs, as a
# dict in the provider's format (such as ``{"type": "web_search"}``).
Tools = Sequence[Tool | dict[str, Any]]

# The conversations, in the object's database.
TABLE = "restate_litellm_messages"


def agent(
    name: str,
    *,
    model: str | Callable[[ObjectContext], str] | None = None,
    system: str | Callable[[ObjectContext], str] | None = None,
    tools: Tools | Callable[[ObjectContext], Tools] = (),
    request: dict[str, Any] | Callable[[ObjectContext], dict[str, Any]] | None = None,
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    ``model``, ``system``, ``tools`` and ``request`` may be functions of the
    invocation's context, or async ones (each user's model, from ``ctx.db``,
    say); ``request``: more of each request, as ``litellm.acompletion``
    takes it, such as ``{"temperature": 0.2, "api_base": "http://localhost:11434"}``.
    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if not callable(model):
        _provider_of(model_of(model))

    async def answer(ctx: ObjectContext, message: str) -> str:
        return await prompt(
            ctx,
            message,
            model=await configured(model, ctx),
            system=await configured(system, ctx),
            tools=await configured(tools, ctx),
            request=await configured(request, ctx),
        )

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        for row in await Conversation(ctx.db, TABLE, ctx.key()).load():
            message = row.message
            if message["role"] == "tool":
                transcript.result(message["tool_call_id"], _text(message.get("content")))
                continue
            transcript.say(message["role"], _text(message.get("content")))
            for call in message.get("tool_calls") or []:
                transcript.call(call["id"], call["function"]["name"], _arguments(call)[0])
        return transcript

    return turns.agent(name, answer, transcript, **options)


async def prompt(
    ctx: Any,
    text: str,
    *,
    model: str | None = None,
    system: str | None = None,
    tools: Tools = (),
    request: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Answer ``text`` in the conversation ``thread`` (by default, the object's
    key), and return the answer. In a handler of your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await litellm.prompt(ctx, f"Summarize what we said about {topic}")

    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    run = _Run(model_of(model), system, list(tools), request or {}, Conversation(ctx.db, TABLE, thread or ctx.key()))
    id = turns.prompt_id(ctx)
    while True:
        rows = await run.conversation.load()
        turns.taken(steered for row in rows if row.prompt == id for steered in row.steered)
        if (answered := answer_of(rows, id)) is not None:
            return answered
        # A prompt that a crash cut off (this one's, or an earlier one's) is finished first.
        pending = pending_of(rows)
        if pending is None:
            await run.conversation.append(id, {"role": "user", "content": text})
            pending = id
        try:
            await run.answer(ctx, pending, [row.message for row in await run.conversation.load()])
        except restate.TerminalError as error:
            if error.status_code == 409:
                await _cancel(run.conversation, pending)
            raise


async def compact(
    ctx: Any,
    instructions: str | None = None,
    *,
    model: str | None = None,
    request: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Summarize the conversation ``thread`` (by default, the object's key) in
    place of it, to keep the model's context short, as ``instructions`` say
    (such as "Keep the times you were asked"); and return the summary. The
    conversation is then the summary, and the next message carries on from
    it. It is compacted once per invocation, however often an attempt runs.
    """
    thread_of = Conversation(ctx.db, TABLE, thread or ctx.key())
    id = turns.prompt_id(ctx)
    rows = await thread_of.load()
    if (done := answer_of(rows, id)) is not None:
        return done
    if not rows:
        return ""
    idle(rows)
    run = _Run(model_of(model), None, [], request or {}, thread_of)
    ask = {"role": "user", "content": f"{conversation.SUMMARIZE} {instructions or ''}".strip()}
    # As text: a request with tool calls and results must define the tools (Anthropic's), which this one has none of.
    summary = _text((await run.reply([*(_as_text(row.message) for row in rows), ask])).get("content"))
    await thread_of.replace(id, f"{conversation.SUMMARIZED}{summary}", answer=summary)
    return summary


async def reset(ctx: Any, handoff: str | None = None, *, thread: str | None = None) -> None:
    """
    Start the conversation ``thread`` (by default, the object's key) over,
    with ``handoff`` as what to carry on with, if given (such as "Alice likes
    short answers").
    """
    await conversation.reset(Conversation(ctx.db, TABLE, thread or ctx.key()), ctx, handoff)


async def _cancel(thread: Conversation, prompt: str) -> None:
    """
    End ``prompt``, which was cancelled: a result for each tool call of its
    last reply that has none, then ``CANCELLED`` as its answer.
    """
    rows = await thread.load()
    last = rows[-1].message if rows else {}
    results = [
        {"role": "tool", "tool_call_id": call["id"], "content": CANCELLED} for call in last.get("tool_calls") or []
    ]
    await thread.append(prompt, *results, {"role": "assistant", "content": CANCELLED}, answer=CANCELLED)


@dataclass
class _Run:
    """A prompt's run: the model's requests and the tools' calls, each stored as it ends."""

    model: str
    system: str | None
    tools: list[Tool | dict[str, Any]]
    request: dict[str, Any]
    conversation: Conversation

    async def answer(self, ctx: Any, prompt: str, messages: list[dict[str, Any]]) -> None:
        """Run ``prompt`` from ``messages`` (the stored ones) until it is answered."""
        while True:
            last = messages[-1]
            calls = (last.get("tool_calls") or []) if last["role"] == "assistant" else []
            if calls:
                # The reply first, then its tools, then the messages steered to the turn, after their results.
                results = [await self.call(call) for call in calls]
                steered = await turns.steering(ctx)
                results += [{"role": "user", "content": s.text} for s in steered]
                await self.conversation.append(prompt, *results, steered=[s.id for s in steered])
                turns.taken(s.id for s in steered)
                messages.extend(results)
            message = await self.reply(messages)
            messages.append(message)
            if message.get("tool_calls"):
                await self.conversation.append(prompt, message)
                continue
            await self.conversation.append(prompt, message, answer=_text(message.get("content")))
            return

    async def reply(self, messages: list[dict[str, Any]]) -> dict[str, Any]:
        """The model's next reply, as the message to store."""
        system = [{"role": "system", "content": self.system}] if self.system is not None else []
        response = await litellm.acompletion(
            **self.request,
            model=self.model,
            messages=[*system, *messages],
            **({"tools": [self.definition(t) for t in self.tools]} if self.tools else {}),
        )
        message = response.choices[0].message.model_dump(exclude_none=True)
        return {k: v for k, v in message.items() if v or k in ("role", "content")}

    def definition(self, tool: Tool | dict[str, Any]) -> dict[str, Any]:
        """``tool`` as the OpenAI format has it."""
        if isinstance(tool, dict):
            return tool
        function = {"name": tool.name, "description": tool.description, "parameters": tool.input.model_json_schema()}
        return {"type": "function", "function": function}

    async def call(self, call: dict[str, Any]) -> dict[str, Any]:
        """The result of tool call ``call``. An error fails the attempt, unless it is a ``restate.TerminalError``."""
        result = {"role": "tool", "tool_call_id": call["id"]}
        name = call["function"]["name"]
        tool = next((t for t in self.tools if isinstance(t, Tool) and t.name == name), None)
        input, malformed = _arguments(call)
        if tool is None or malformed:
            return {**result, "content": f"Error: no tool {name}" if tool is None else f"Error: {malformed}"}
        try:
            output = await tool.invoke(input, call["id"])
        except ValidationError as error:
            return {**result, "content": f"Error: {error}"}
        except restate.TerminalError as error:
            if error.status_code == 409:
                raise
            return {**result, "content": error.message}
        return {**result, "content": text_of(output)}


def _provider_of(model: str) -> str:
    """The provider of ``model``, as LiteLLM resolves it: one it does not know fails here, not at the first message."""
    try:
        return litellm.get_llm_provider(model)[1]
    except litellm.BadRequestError:
        source = " (from MODEL)" if os.environ.get("MODEL") == model else ""
        raise ModelError(
            f'LiteLLM knows no provider of the model "{model}"{source}: it takes provider/model-id'
        ) from None


def _arguments(call: dict[str, Any]) -> tuple[dict[str, Any], str | None]:
    """A tool call's input, from its arguments (JSON, as the model wrote them), or why they are not an object."""
    try:
        input = json.loads(call["function"].get("arguments") or "{}")
    except ValueError as error:
        return {}, f"the arguments are not JSON: {error}"
    return (input, None) if isinstance(input, dict) else ({}, "the arguments are not a JSON object")


def _as_text(message: dict[str, Any]) -> dict[str, Any]:
    """``message`` with its tool calls and results as text: ``[called move_money {"amount": 5}]``."""
    if message["role"] == "tool":
        return {"role": "user", "content": f"[result: {_text(message.get('content'))}]"}
    if not message.get("tool_calls"):
        return message
    calls = [f"[called {c['function']['name']} {c['function']['arguments']}]" for c in message["tool_calls"]]
    return {"role": message["role"], "content": "\n".join([_text(message.get("content")), *calls]).strip()}


def _text(content: str | list[dict[str, Any]] | None) -> str:
    """The text of a message's content: a string, or the text of its parts."""
    if content is None or isinstance(content, str):
        return content or ""
    return "".join(part.get("text", "") for part in content if part.get("type") == "text")

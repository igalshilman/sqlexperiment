"""
Claude agents in Restate virtual objects, on the Anthropic SDK: Claude with
nothing in between, the API's own messages stored in the object's database::

    from restate_agents import claude, serve, tool

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = claude.agent("assistant", system="Answer briefly.", tools=[get_weather])
    serve(assistant)

The model is ``model`` (``anthropic/model-id``), or else MODEL, or else
anthropic/claude-sonnet-5-5, with ANTHROPIC_API_KEY. Each request is the
API's own JSON, as the host's ``claude.messages`` binding of the durable
functions sends it: with ``fallbacks: "default"`` unless ``request`` says
otherwise, and more of it from ``request``.

The loop is the agent's own, not the SDK's tool runner
(``client.beta.messages.tool_runner``): the runner gives a tool its input
but not its call's id, and cannot start from a stored reply whose tools did
not run.

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
- A message steered to a turn (``steer``) joins the tools' results, and the
  model reads it with them.
"""

from __future__ import annotations

import inspect
import json
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import Any

import restate_fns as restate
from anthropic import AsyncAnthropic
from pydantic import ValidationError
from restate_fns import ObjectContext

from restate_agents import conversation, turns
from restate_agents.common import CLAUDE_MODEL, Transcript, configured, model_id_of, model_of
from restate_agents.conversation import Conversation, answer_of, idle, pending_of
from restate_agents.object import Object
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DECLINED", "DEFAULT_MODEL", "REQUEST", "agent", "compact", "prompt", "reset"]

DEFAULT_MODEL = CLAUDE_MODEL
"""The model of an agent given none, and no MODEL."""

# What each request has unless ``request`` says otherwise, as the host's
# ``claude.messages`` binding sends it: a declined request is retried on
# another model by the API itself.
REQUEST: dict[str, Any] = {"max_tokens": 16000, "fallbacks": "default"}
_FALLBACK_BETA = "server-side-fallback-2026-07-01"

# The answer to a request the model declined, on every model it was tried on.
DECLINED = "Declined: the model would not answer this."

# A tool: a ``tool`` or one of Restate's; an SDK tool (``@beta_async_tool``,
# ``@beta_tool``), which gets its input only; or a tool the API runs (a dict,
# such as ``{"type": "web_search_20260209", "name": "web_search"}``).
Tools = Sequence[Tool | Any]

# The conversations, in the object's database.
TABLE = "restate_claude_messages"


def agent(
    name: str,
    *,
    model: str | Callable[[ObjectContext], str] | None = None,
    system: str | Callable[[ObjectContext], str] | None = None,
    tools: Tools | Callable[[ObjectContext], Tools] = (),
    client: AsyncAnthropic | None = None,
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
    invocation's context, or async ones (each user's, from ``ctx.db``, say);
    ``client``: the Anthropic client (a base URL
    or a gateway of your own), by default one with ANTHROPIC_API_KEY;
    ``request``: more of each request, as the API takes it, such as
    ``{"max_tokens": 4096, "output_config": {"effort": "low"}}`` (over
    ``REQUEST``). ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if not callable(model):
        model_id_of(model_of(model, DEFAULT_MODEL), "anthropic")

    async def answer(ctx: ObjectContext, message: str) -> str:
        return await prompt(
            ctx,
            message,
            model=await configured(model, ctx),
            system=await configured(system, ctx),
            tools=await configured(tools, ctx),
            client=client,
            request=await configured(request, ctx),
        )

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        for row in await Conversation(ctx.db, TABLE, ctx.key()).load():
            content = row.message["content"]
            transcript.say(row.message["role"], _text(content))
            for block in [] if isinstance(content, str) else content:
                if block["type"] == "tool_use":
                    transcript.call(block["id"], block["name"], block["input"])
                elif block["type"] == "tool_result":
                    transcript.result(
                        block["tool_use_id"], _text(block.get("content", "")), block.get("is_error", False)
                    )
        return transcript

    return turns.agent(name, answer, transcript, **options)


async def prompt(
    ctx: Any,
    text: str,
    *,
    model: str | None = None,
    system: str | None = None,
    tools: Tools = (),
    client: AsyncAnthropic | None = None,
    request: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Answer ``text`` in the conversation ``thread`` (by default, the object's
    key), and return the answer. In a handler of your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await claude.prompt(ctx, f"Summarize what we said about {topic}")

    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    run = _Run.of(
        Conversation(ctx.db, TABLE, thread or ctx.key()),
        model=model,
        client=client,
        request=request,
        system=system,
        tools=tools,
    )
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
    client: AsyncAnthropic | None = None,
    request: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Summarize the conversation ``thread`` (by default, the object's key) in
    place of it, to keep the model's context short, as ``instructions`` say
    (such as "Keep the times you were asked"); and return the summary. In a
    handler of your own::

        @assistant.handler(message=True)
        async def compact(ctx, instructions: str) -> str:
            return await claude.compact(ctx, instructions)

    The conversation is then the summary, and the next message carries on
    from it. It is compacted once per invocation, however often an attempt
    runs.
    """
    thread_of = Conversation(ctx.db, TABLE, thread or ctx.key())
    id = turns.prompt_id(ctx)
    rows = await thread_of.load()
    if (done := answer_of(rows, id)) is not None:
        return done
    if not rows:
        return ""
    idle(rows)
    run = _Run.of(thread_of, model=model, client=client, request=request)
    ask = {"role": "user", "content": f"{conversation.SUMMARIZE} {instructions or ''}".strip()}
    # As text: a request with tool calls and results must define the tools, which this one has none of.
    reply = await run.reply([*(_as_text(row.message) for row in rows), ask])
    summary = _text([block.model_dump(exclude_none=True) for block in reply.content])
    await thread_of.replace(id, f"{conversation.SUMMARIZED}{summary}", answer=summary)
    return summary


async def reset(ctx: Any, handoff: str | None = None, *, thread: str | None = None) -> None:
    """
    Start the conversation ``thread`` (by default, the object's key) over,
    with ``handoff`` as what to carry on with, if given (such as "Alice likes
    short answers"). In a handler of your own::

        @assistant.handler(message=True)
        async def reset(ctx, handoff: str) -> None:
            await claude.reset(ctx, handoff)
    """
    await conversation.reset(Conversation(ctx.db, TABLE, thread or ctx.key()), ctx, handoff)


async def _cancel(thread: Conversation, prompt: str) -> None:
    """
    End ``prompt``, which was cancelled: a result for each tool call of its
    last reply that has none, then ``CANCELLED`` as its answer.
    """
    rows = await thread.load()
    last = rows[-1].message if rows else None
    calls = (
        [b for b in last["content"] if b["type"] == "tool_use"]
        if last and last["role"] == "assistant" and isinstance(last["content"], list)
        else []
    )
    results = [{"type": "tool_result", "tool_use_id": c["id"], "content": CANCELLED, "is_error": True} for c in calls]
    replies = [{"role": "user", "content": results}] if results else []
    await thread.append(prompt, *replies, {"role": "assistant", "content": CANCELLED}, answer=CANCELLED)


@dataclass
class _Run:
    """A prompt's run: the model's requests and the tools' calls, each stored as it ends."""

    client: AsyncAnthropic
    model: str
    system: str | None
    tools: list[Tool | Any]
    request: dict[str, Any]
    conversation: Conversation

    @classmethod
    def of(
        cls,
        conversation: Conversation,
        *,
        model: str | None,
        client: AsyncAnthropic | None,
        request: dict[str, Any] | None,
        system: str | None = None,
        tools: Tools = (),
    ) -> _Run:
        """A run of ``conversation``, with the options of ``prompt``: over ``REQUEST``, and the shared client."""
        return cls(
            client=client or _client(),
            model=model_id_of(model_of(model, DEFAULT_MODEL), "anthropic"),
            system=system,
            tools=list(tools),
            request={**REQUEST, **(request or {})},
            conversation=conversation,
        )

    async def answer(self, ctx: Any, prompt: str, messages: list[dict[str, Any]]) -> None:
        """Run ``prompt`` from ``messages`` (the stored ones) until it is answered."""
        while True:
            last = messages[-1]
            calls = [b for b in last["content"] if b["type"] == "tool_use"] if last["role"] == "assistant" else []
            if calls:
                # The reply first, then its tools, then the messages steered to the turn, with their results.
                content = [await self.call(call) for call in calls]
                steered = await turns.steering(ctx)
                content += [{"type": "text", "text": s.text} for s in steered]
                results = {"role": "user", "content": content}
                await self.conversation.append(prompt, results, steered=[s.id for s in steered])
                turns.taken(s.id for s in steered)
                messages.append(results)
            reply = await self.reply(messages)
            message = {"role": "assistant", "content": [block.model_dump(exclude_none=True) for block in reply.content]}
            messages.append(message)
            # A paused turn (a tool the API runs) goes on with the next request.
            if reply.stop_reason == "pause_turn" or any(b["type"] == "tool_use" for b in message["content"]):
                await self.conversation.append(prompt, message)
                continue
            answer = _text(message["content"]) or (DECLINED if reply.stop_reason == "refusal" else "")
            await self.conversation.append(prompt, message, answer=answer)
            return

    async def reply(self, messages: list[dict[str, Any]]) -> Any:
        """The model's next reply."""
        request = dict(self.request)
        if request.get("fallbacks"):
            request["betas"] = [*request.get("betas", []), _FALLBACK_BETA]
        return await self.client.beta.messages.create(
            **request,
            model=self.model,
            messages=messages,
            **({"system": self.system} if self.system is not None else {}),
            **({"tools": [self.definition(t) for t in self.tools]} if self.tools else {}),
        )

    def definition(self, tool: Tool | Any) -> Any:
        """``tool`` as the API takes it."""
        if isinstance(tool, Tool):
            return {"name": tool.name, "description": tool.description, "input_schema": tool.input.model_json_schema()}
        return tool if isinstance(tool, dict) else tool.to_dict()

    async def call(self, call: dict[str, Any]) -> dict[str, Any]:
        """The result of tool call ``call``. An error fails the attempt, unless it is a ``restate.TerminalError``."""
        result = {"type": "tool_result", "tool_use_id": call["id"]}
        tool = next((t for t in self.tools if _name(t) == call["name"]), None)
        if tool is None:
            return {**result, "content": f"Error: no tool {call['name']}", "is_error": True}
        try:
            if isinstance(tool, Tool):
                output = await tool.invoke(call["input"], call["id"])
            else:
                output = tool.call(call["input"])
                output = await output if inspect.isawaitable(output) else output
        except ValidationError as error:
            return {**result, "content": f"Error: {error}", "is_error": True}
        except restate.TerminalError as error:
            if error.status_code == 409:
                raise
            return {**result, "content": error.message, "is_error": True}
        return {**result, "content": output if _blocks(output) else text_of(output)}


# The content blocks a tool's result may be, as the API takes them; any other result is text.
_BLOCKS = {"text", "image", "document", "search_result"}


def _blocks(output: Any) -> bool:
    """Whether ``output`` is content blocks (an image, say), not a value of the tool's, such as a list of rows."""
    return (
        isinstance(output, list)
        and bool(output)
        and all(isinstance(b, dict) and b.get("type") in _BLOCKS for b in output)
    )


def _name(tool: Tool | Any) -> str | None:
    """The name of ``tool``, of any kind ``Tools`` takes."""
    return tool.get("name") if isinstance(tool, dict) else getattr(tool, "name", None)


def _as_text(message: dict[str, Any]) -> dict[str, Any]:
    """``message`` with its tool calls and results as text: ``[called move_money {"amount": 5}]``."""
    if isinstance(message["content"], str):
        return message
    said = []
    for block in message["content"]:
        match block.get("type"):
            case "text":
                said.append(block["text"])
            case "tool_use":
                said.append(f"[called {block['name']} {json.dumps(block['input'])}]")
            case "tool_result":
                said.append(f"[result: {_text(block.get('content') or '')}]")
    return {"role": message["role"], "content": "\n".join(said) or "[no text]"}


def _text(content: str | list[dict[str, Any]]) -> str:
    """The text of a message's content, without its tool calls and results."""
    if isinstance(content, str):
        return content
    return "".join(block.get("text", "") for block in content if block.get("type") == "text")


# The client of the agents without one of their own, made when first used.
_shared: AsyncAnthropic | None = None


def _client() -> AsyncAnthropic:
    global _shared
    _shared = _shared or AsyncAnthropic()
    return _shared

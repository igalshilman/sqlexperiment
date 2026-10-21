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

It is the SDK tool runner's loop (``client.beta.messages.tool_runner``), of
its own: the runner gives a tool its input but not its call's id, and cannot
start from a stored reply whose tools did not run.

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
from dataclasses import dataclass, field
from typing import Any

import restate_sql as restate
from anthropic import AsyncAnthropic
from pydantic import ValidationError
from restate_sql import ObjectContext

from restate_agents import turns
from restate_agents.common import Messages, MessagesRequest, Transcript, has_table, model_id_of, model_of
from restate_agents.object import Object
from restate_agents.tools import Tool, text_of
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DECLINED", "DEFAULT_MODEL", "REQUEST", "agent", "compact", "prompt", "reset"]

DEFAULT_MODEL = "anthropic/claude-sonnet-5-5"

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
    obj = Object(name, **options)

    async def answer(ctx: ObjectContext, message: str) -> str:
        async def given(option: Any) -> Any:
            value = option(ctx) if callable(option) else option
            return await value if inspect.isawaitable(value) else value

        return await prompt(
            ctx,
            message,
            model=await given(model),
            system=await given(system),
            tools=await given(tools),
            client=client,
            request=await given(request),
        )

    async def messages(ctx: ObjectContext, request: MessagesRequest | None = None) -> Messages:
        """The conversation so far, with the tool calls."""
        transcript = Transcript()
        if not await has_table(ctx.db, "claude_messages"):
            return transcript.page(request)
        for row in await _Conversation(ctx.db, ctx.key()).load():
            content = row.message["content"]
            transcript.say(row.message["role"], _text(content))
            for block in [] if isinstance(content, str) else content:
                if block["type"] == "tool_use":
                    transcript.call(block["id"], block["name"], block["input"])
                elif block["type"] == "tool_result":
                    transcript.result(
                        block["tool_use_id"], _text(block.get("content", "")), block.get("is_error", False)
                    )
        return transcript.page(request)

    turns.chat(obj, answer)
    obj.handler("messages", kind="shared")(messages)
    return obj


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
    run = _Run(
        client=client or _client(),
        model=model_id_of(model_of(model, DEFAULT_MODEL), "anthropic"),
        system=system,
        tools=list(tools),
        request={**REQUEST, **(request or {})},
        conversation=_Conversation(ctx.db, thread or ctx.key()),
    )
    id = turns.prompt_id(ctx)
    while True:
        rows = await run.conversation.load()
        turns.taken(steered for row in rows if row.prompt == id for steered in row.steered)
        if (answered := _answer(rows, id)) is not None:
            return answered
        # A prompt that a crash cut off (this one's, or an earlier one's) is finished first.
        pending = rows[-1].prompt if rows and rows[-1].answer is None else None
        if pending is None:
            await run.conversation.append(id, {"role": "user", "content": text})
            pending = id
        try:
            await run.answer(ctx, pending, [row.message for row in await run.conversation.load()])
        except restate.TerminalError as error:
            if error.status_code == 409:
                await run.conversation.cancel(pending)
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
    conversation = _Conversation(ctx.db, thread or ctx.key())
    id = turns.prompt_id(ctx)
    rows = await conversation.load()
    if (done := _answer(rows, id)) is not None:
        return done
    if not rows:
        return ""
    _idle(rows)
    asking = (
        "Summarize our conversation so far, in place of it: what was asked and answered, and what to carry on with."
    )
    run = _Run(
        client=client or _client(),
        model=model_id_of(model_of(model, DEFAULT_MODEL), "anthropic"),
        system=None,
        tools=[],
        request={**REQUEST, **(request or {})},
        conversation=conversation,
    )
    ask = {"role": "user", "content": f"{asking} {instructions or ''}".strip()}
    reply = await run.reply([*(row.message for row in rows), ask])
    summary = _text([block.model_dump(exclude_none=True) for block in reply.content])
    await conversation.replace(id, f"Our conversation so far, summarized:\n\n{summary}", answer=summary)
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
    conversation = _Conversation(ctx.db, thread or ctx.key())
    _idle(await conversation.load())
    note = f"Before this conversation: {handoff}" if handoff else ""
    await conversation.replace(turns.prompt_id(ctx), note, answer=handoff or "")


def _answer(rows: list[_Row], prompt: str) -> str | None:
    """The answer to ``prompt``, if it was answered."""
    return next((row.answer for row in rows if row.prompt == prompt and row.answer is not None), None)


def _idle(rows: list[_Row]) -> None:
    """Refuse to change a conversation whose last message is still being answered."""
    if rows and rows[-1].answer is None:
        raise restate.TerminalError("a message is being answered: change the conversation once it is")


@dataclass
class _Row:
    """A message of the conversation: its prompt, the answer if it answers it, and the messages steered into it."""

    prompt: str
    message: dict[str, Any]
    answer: str | None = None
    steered: list[str] = field(default_factory=list)


class _Conversation:
    """A conversation in the object's database: the API's messages, a row each, in order."""

    SCHEMA = """
    CREATE TABLE IF NOT EXISTS claude_messages (
      thread TEXT NOT NULL,
      n INTEGER NOT NULL,
      prompt TEXT NOT NULL,
      message TEXT NOT NULL,
      answer TEXT,
      steered TEXT,
      PRIMARY KEY (thread, n)
    )
    """

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self.length = 0

    async def load(self) -> list[_Row]:
        if not await has_table(self.db, "claude_messages"):
            await self.db.execute(self.SCHEMA)
        rows = await self.db.query(
            "SELECT prompt, message, answer, steered FROM claude_messages WHERE thread = ? ORDER BY n", [self.thread]
        )
        self.length = len(rows)
        return [
            _Row(prompt, json.loads(message), answer, json.loads(steered or "[]"))
            for prompt, message, answer, steered in rows
        ]

    async def append(
        self, prompt: str, *messages: dict[str, Any], answer: str | None = None, steered: Sequence[str] = ()
    ) -> None:
        """Store ``messages`` of ``prompt`` in one transaction: the last with its ``answer``, and ``steered``."""
        async with self.db.transaction() as tx:
            for i, message in enumerate(messages):
                last = i == len(messages) - 1
                await tx.execute(
                    "INSERT INTO claude_messages VALUES (?, ?, ?, ?, ?, ?)",
                    [
                        self.thread,
                        self.length,
                        prompt,
                        json.dumps(message),
                        answer if last else None,
                        json.dumps(list(steered)) if last and steered else None,
                    ],
                )
                self.length += 1
        await turns.publish()

    async def replace(self, prompt: str, note: str, *, answer: str) -> None:
        """
        The conversation, in place of all it was, in one transaction: ``note``
        and its acknowledgment, as ``prompt``'s, answered ``answer``; or
        nothing, without a note.
        """
        messages = [{"role": "user", "content": note}, {"role": "assistant", "content": _CARRY_ON}] if note else []
        async with self.db.transaction() as tx:
            await tx.execute("DELETE FROM claude_messages WHERE thread = ?", [self.thread])
            for n, message in enumerate(messages):
                await tx.execute(
                    "INSERT INTO claude_messages VALUES (?, ?, ?, ?, ?, NULL)",
                    [self.thread, n, prompt, json.dumps(message), answer if n == len(messages) - 1 else None],
                )
        self.length = len(messages)
        await turns.publish()

    async def cancel(self, prompt: str) -> None:
        """
        End ``prompt``, which was cancelled: a result for each tool call of its
        last reply that has none, then ``CANCELLED`` as its answer.
        """
        rows = await self.load()
        last = rows[-1].message if rows else None
        calls = (
            [b for b in last["content"] if b["type"] == "tool_use"]
            if last and last["role"] == "assistant" and isinstance(last["content"], list)
            else []
        )
        results = [
            {"type": "tool_result", "tool_use_id": c["id"], "content": CANCELLED, "is_error": True} for c in calls
        ]
        replies = [{"role": "user", "content": results}] if results else []
        await self.append(prompt, *replies, {"role": "assistant", "content": CANCELLED}, answer=CANCELLED)


@dataclass
class _Run:
    """A prompt's run: the model's requests and the tools' calls, each stored as it ends."""

    client: AsyncAnthropic
    model: str
    system: str | None
    tools: list[Tool | Any]
    request: dict[str, Any]
    conversation: _Conversation

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
                input = tool.input.model_validate(call["input"])
                output = await tool.run({name: getattr(input, name) for name in type(input).model_fields}, call["id"])
            else:
                output = tool.call(call["input"])
                output = await output if inspect.isawaitable(output) else output
        except ValidationError as error:
            return {**result, "content": f"Error: {error}", "is_error": True}
        except restate.TerminalError as error:
            if error.status_code == 409:
                raise
            return {**result, "content": error.message, "is_error": True}
        return {**result, "content": output if isinstance(output, list) else text_of(output)}


def _name(tool: Tool | Any) -> str | None:
    if isinstance(tool, Tool | dict):
        return tool.name if isinstance(tool, Tool) else tool.get("name")
    return getattr(tool, "name", None)


def _text(content: str | list[dict[str, Any]]) -> str:
    """The text of a message's content, without its tool calls and results."""
    if isinstance(content, str):
        return content
    return "".join(block.get("text", "") for block in content if block.get("type") == "text")


# The model's acknowledgment of a summary or a handoff, which the conversation then carries on from.
_CARRY_ON = "Understood: I will carry on from there."

# The client of the agents without one of their own, made when first used.
_shared: AsyncAnthropic | None = None


def _client() -> AsyncAnthropic:
    global _shared
    _shared = _shared or AsyncAnthropic()
    return _shared

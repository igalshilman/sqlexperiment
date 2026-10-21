"""
Pydantic AI agents in Restate virtual objects: each conversation is in the
object's database, stored a step at a time::

    from restate_agents import serve, tool
    from restate_agents.pydantic_ai import agent

    @tool
    def get_weather(city: str) -> str:
        '''The current weather in a city.'''
        return f"Sunny and 21°C in {city}"

    assistant = agent("assistant", tools=[get_weather])
    serve(assistant)

The usual agent is a Pydantic AI ``Agent`` with ``model`` (``provider/model-id``,
or a Pydantic AI model), or else MODEL, or else anthropic/claude-sonnet-5-5,
with ANTHROPIC_API_KEY; ``instructions``; and ``tools``. Or bring an ``Agent``
of your own: ``agent(name, Agent(...))``, with ``toolset(tools)`` for the
tools here (``tool``, ``call``, ``signal``, ...).

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and the run carries on from the last step stored:

- The conversation is stored before each model request, and with each
  model response, in order with everything else the handler does. So the
  tool calls a model made are stored before any tool runs, and a crash
  during the tools runs them again with the same tool call ids, which a tool
  passes on to stay idempotent (its ``call_id``).
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A message is answered once per invocation: it goes into the conversation
  marked with the invocation's id, and a retried attempt that finds it there
  finishes its run, or returns its answer.
- A cancelled turn (``stop``) ends: each of its cut-off tool calls, and the
  turn, are answered "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) joins its conversation after the
  next tool call, and the model reads it with the results.
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import replace
from typing import Any

import restate_fns as restate
from pydantic import TypeAdapter
from pydantic_ai import Agent, RunContext
from pydantic_ai import Tool as PydanticTool
from pydantic_ai.capabilities import AbstractCapability
from pydantic_ai.messages import (
    ModelMessage,
    ModelRequest,
    ModelResponse,
    RetryPromptPart,
    TextPart,
    ToolCallPart,
    ToolReturnPart,
    UserPromptPart,
)
from pydantic_ai.models import Model, ModelRequestContext
from pydantic_ai.toolsets import FunctionToolset
from restate_fns import ObjectContext

from restate_agents import turns
from restate_agents.common import Messages, MessagesRequest, Transcript, has_table, model_of
from restate_agents.object import Object, context
from restate_agents.tools import Tool
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DEFAULT_MODEL", "agent", "prompt", "toolset"]

DEFAULT_MODEL = "anthropic/claude-sonnet-5-5"

# The metadata of the conversation's messages (which models do not read): a
# prompt's request, the request of the tool results that the messages steered
# to the turn join, and the response that answers a prompt.
PROMPT = "restate_agents_prompt"
STEER = "restate_agents_steer"
ANSWERS = "restate_agents_answers"

Tools = Sequence[Tool | PydanticTool | Callable[..., Any]]


def agent(
    name: str,
    pydantic_agent: Agent[Any, Any] | Callable[[ObjectContext], Agent[Any, Any]] | None = None,
    *,
    model: str | Model | Callable[[ObjectContext], str | Model] | None = None,
    instructions: str | None = None,
    tools: Tools = (),
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    The usual agent takes ``model``: ``provider/model-id`` (Pydantic AI's
    providers, such as ``anthropic`` or ``openai``), a Pydantic AI model, or a
    function of the invocation's context giving either; ``instructions``; and
    ``tools``: ``tool``s, Restate's tools, and Pydantic AI's tools or tool
    functions. Or ``pydantic_agent``: an ``Agent`` of your own, or a function
    of the context giving one, whose output is text.

    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if pydantic_agent is not None and (model is not None or instructions is not None or tools):
        raise TypeError(
            f"{name}: an agent takes an Agent, or the usual agent's model, instructions and tools; not both"
        )
    usual = _usual(instructions, tools) if pydantic_agent is None else None
    obj = Object(name, **options)

    async def answer(ctx: ObjectContext, message: str) -> str:
        if usual is not None:
            return await prompt(ctx, usual, message, model=_model(model, ctx))
        chosen = pydantic_agent if isinstance(pydantic_agent, Agent) else pydantic_agent(ctx)
        return await prompt(ctx, chosen, message)

    async def messages(ctx: ObjectContext, request: MessagesRequest | None = None) -> Messages:
        """The conversation so far, with the tool calls."""
        transcript = Transcript()
        if not await has_table(ctx.db, "pydantic_ai_messages"):
            return transcript.page(request)
        for m in await _Conversation(ctx.db, ctx.key()).load():
            for part in m.parts:
                if isinstance(part, UserPromptPart):
                    transcript.say("user", part.content if isinstance(part.content, str) else "")
                elif isinstance(part, TextPart):
                    transcript.say("assistant", part.content)
                elif isinstance(part, ToolCallPart):
                    transcript.call(part.tool_call_id, part.tool_name, part.args_as_dict())
                elif isinstance(part, ToolReturnPart | RetryPromptPart) and part.tool_call_id:
                    transcript.result(part.tool_call_id, part.model_response_str(), isinstance(part, RetryPromptPart))
        return transcript.page(request)

    turns.chat(obj, answer)
    obj.handler("messages", kind="shared")(messages)
    return obj


async def prompt(ctx: Any, agent: Agent[Any, Any], text: str, *, thread: str | None = None, **run: Any) -> Any:
    """
    Answer ``text`` with ``agent``, in the conversation ``thread`` (by
    default, the object's key), and return its output. In a handler of your own::

        @assistant.handler
        async def summarize(ctx, topic: str) -> str:
            return await prompt(ctx, summarizer, f"Summarize what we said about {topic}")

    ``run``: the options of ``Agent.run``, such as ``deps`` or ``usage_limits``.
    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    conversation = _Conversation(ctx.db, thread or ctx.key())
    id = turns.prompt_id(ctx)
    while True:
        history = await conversation.load()
        last = next((i for i in reversed(range(len(history))) if _prompt(history[i])), None)
        if last is not None and _answer(history, last) is None:
            # A run that a crash cut off (this prompt's, or an earlier one's): finish it first.
            await _run(conversation, agent, history, _prompt(history[last]), run)
            continue
        asked = next((i for i, m in enumerate(history) if _prompt(m) == id), None)
        if asked is not None:
            answer = _answer(history, asked)
            assert answer is not None
            turns.taken(steer for m in history[asked:answer] for steer in _steered(m))
            # Its output, from its answer, without asking the model again.
            return (await agent.run(message_history=history[: answer + 1], **run)).output
        request = ModelRequest(parts=[UserPromptPart(text)], metadata={PROMPT: id})
        return await _run(conversation, agent, [*history, request], id, run)


def toolset(tools: Tools) -> FunctionToolset[Any]:
    """
    ``tools`` as a Pydantic AI toolset, for an ``Agent`` of your own::

        Agent("anthropic:claude-sonnet-5-5", toolsets=[toolset([refund, call(status)])])

    ``tools``: ``tool``s, Restate's tools, and Pydantic AI's tools or tool functions.
    """
    return FunctionToolset([_pydantic(t) if isinstance(t, Tool) else t for t in tools])


class _Conversation:
    """
    A conversation in the object's database, a message per row. Saving it
    writes the messages that changed since it was loaded or saved, in one
    transaction.
    """

    SCHEMA = """
    CREATE TABLE IF NOT EXISTS pydantic_ai_messages (
      thread TEXT NOT NULL,
      n INTEGER NOT NULL,
      message TEXT NOT NULL,
      PRIMARY KEY (thread, n)
    )
    """

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self._saved: list[str] = []

    async def load(self) -> list[ModelMessage]:
        if not await has_table(self.db, "pydantic_ai_messages"):
            await self.db.execute(self.SCHEMA)
        rows = await self.db.query(
            "SELECT message FROM pydantic_ai_messages WHERE thread = ? ORDER BY n", [self.thread]
        )
        self._saved = [row[0] for row in rows]
        return [_MESSAGE.validate_json(m) for m in self._saved]

    async def save(self, messages: Sequence[ModelMessage]) -> None:
        dumped = [_MESSAGE.dump_json(m).decode() for m in messages]
        same = next((i for i, (a, b) in enumerate(zip(dumped, self._saved, strict=False)) if a != b), None)
        first = same if same is not None else min(len(dumped), len(self._saved))
        if first == len(dumped) == len(self._saved):
            return
        async with self.db.transaction() as tx:
            await tx.execute("DELETE FROM pydantic_ai_messages WHERE thread = ? AND n >= ?", [self.thread, first])
            for n in range(first, len(dumped)):
                await tx.execute("INSERT INTO pydantic_ai_messages VALUES (?, ?, ?)", [self.thread, n, dumped[n]])
        self._saved = dumped
        await turns.publish()


_MESSAGE: TypeAdapter[ModelMessage] = TypeAdapter(ModelMessage)


class _Restate(AbstractCapability[Any]):
    """
    A run in a Restate handler: its conversation stored before each model
    request and with each response, the messages steered to its turn added
    after each tool call, and a tool's ``restate.TerminalError`` its answer.
    """

    def __init__(self, conversation: _Conversation) -> None:
        self.conversation = conversation

    async def before_model_request(
        self, ctx: RunContext[Any], request_context: ModelRequestContext
    ) -> ModelRequestContext:
        results = ctx.messages[-1]
        if _has_results(results) and not _steered(results):
            has = {id for m in ctx.messages for id in _steered(m)}
            steered = [s for s in await turns.steering(context()) if s.id not in has]
            turns.taken(s.id for s in steered)
            if steered:
                # With the results, in their request: Pydantic AI merges requests in a row, and their metadata.
                parts = [*results.parts, *(UserPromptPart(s.text) for s in steered)]
                metadata = {**(results.metadata or {}), STEER: [s.id for s in steered]}
                ctx.messages[-1] = request_context.messages[-1] = replace(results, parts=parts, metadata=metadata)
        await self.conversation.save(ctx.messages)
        return request_context

    async def after_model_request(
        self, ctx: RunContext[Any], *, request_context: ModelRequestContext, response: ModelResponse
    ) -> ModelResponse:
        await self.conversation.save([*ctx.messages, response])
        return response

    async def on_tool_execute_error(self, ctx: RunContext[Any], *, error: Exception, **_: Any) -> Any:
        if isinstance(error, restate.TerminalError) and error.status_code != 409:
            return error.message
        raise error


async def _run(
    conversation: _Conversation, agent: Agent[Any, Any], history: list[ModelMessage], id: str, run: dict[str, Any]
) -> Any:
    """Run ``agent`` on from ``history`` to the answer of prompt ``id``, and mark the answer as its."""
    try:
        result = await agent.run(message_history=history, capabilities=[_Restate(conversation)], **run)
    except restate.TerminalError as error:
        if error.status_code == 409:
            await _cancel(conversation, id)
        raise
    messages = result.all_messages()
    answer = next(m for m in reversed(messages) if isinstance(m, ModelResponse))
    answer.metadata = {**(answer.metadata or {}), ANSWERS: id}
    await conversation.save(messages)
    return result.output


async def _cancel(conversation: _Conversation, id: str) -> None:
    """
    End the run of prompt ``id``, which was cancelled: a result for each tool
    call of its last response that has none, then ``CANCELLED`` as its answer.
    """
    messages = await conversation.load()
    asked = next((i for i, m in enumerate(messages) if _prompt(m) == id), None)
    if asked is None:
        return
    if isinstance(last := messages[-1], ModelResponse) and last.tool_calls:
        returns = [ToolReturnPart(call.tool_name, CANCELLED, call.tool_call_id) for call in last.tool_calls]
        messages.append(ModelRequest(parts=returns))
    messages.append(ModelResponse(parts=[TextPart(CANCELLED)], metadata={ANSWERS: id}))
    await conversation.save(messages)


def _answer(history: list[ModelMessage], asked: int) -> int | None:
    """The index of the response that answers the prompt at ``asked``, if it was answered."""
    return next((i for i in range(asked, len(history)) if (history[i].metadata or {}).get(ANSWERS)), None)


def _prompt(message: ModelMessage) -> str | None:
    """The id of the prompt that ``message`` is, if it is one."""
    return (message.metadata or {}).get(PROMPT) if isinstance(message, ModelRequest) else None


def _steered(message: ModelMessage) -> list[str]:
    """The ids of the messages steered to the turn that ``message`` gives the model."""
    return (message.metadata or {}).get(STEER, [])


def _has_results(message: ModelMessage) -> bool:
    """Whether ``message`` gives the model tool results: the request after a tool call."""
    return isinstance(message, ModelRequest) and any(
        isinstance(p, ToolReturnPart | RetryPromptPart) for p in message.parts
    )


def _usual(instructions: str | None, tools: Tools) -> Agent[None, str]:
    """The usual agent: its model given each run."""
    own = [t for t in tools if isinstance(t, Tool)]
    return Agent(
        instructions=instructions,
        tools=[t for t in tools if not isinstance(t, Tool)],
        toolsets=[toolset(own)] if own else None,
    )


def _model(model: str | Model | Callable[[ObjectContext], str | Model] | None, ctx: ObjectContext) -> str | Model:
    """The model, as Pydantic AI names it: ``provider:model-id``."""
    chosen = model(ctx) if callable(model) and not isinstance(model, Model) else model
    if isinstance(chosen, Model):
        return chosen
    named = model_of(chosen, DEFAULT_MODEL)
    return named if ":" in named else named.replace("/", ":", 1)


def _pydantic(t: Tool) -> PydanticTool[Any]:
    """``t`` as a Pydantic AI tool, which gets its tool call's id."""

    async def run(ctx: RunContext[Any], **args: Any) -> Any:
        input = t.input.model_validate(args)
        return await t.run({name: getattr(input, name) for name in type(input).model_fields}, ctx.tool_call_id or "")

    return PydanticTool.from_schema(run, t.name, t.description, t.input.model_json_schema(), takes_ctx=True)

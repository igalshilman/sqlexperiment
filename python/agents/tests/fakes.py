"""
A scripted model and a fake object context, for the agents' tests without a
Restate server or an API key.

The model, the teller, moves money: "move 5 from 1 to 2" makes a transfer
tool call, and the transfer's result is its answer. The context runs an
invocation's attempts over an object's state, as Restate would: its writes
are stored in order, and an attempt can crash at any state request.
"""

from __future__ import annotations

import asyncio
import itertools
import json
import re
import uuid
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import Annotated, Any

from anthropic.types import Message, TextBlock, ToolUseBlock, Usage
from langchain_core.callbacks import AsyncCallbackManagerForLLMRun, CallbackManagerForLLMRun
from langchain_core.language_models import BaseChatModel
from langchain_core.messages import AIMessage, BaseMessage, HumanMessage, SystemMessage, ToolMessage
from langchain_core.outputs import ChatGeneration, ChatResult
from langchain_core.tools import InjectedToolCallId, tool
from pydantic_ai.messages import (
    ModelMessage,
    ModelResponse,
    RetryPromptPart,
    TextPart,
    ToolCallPart,
    ToolReturnPart,
    UserPromptPart,
)
from pydantic_ai.models.function import AgentInfo, FunctionModel
from restate.server_context import _restate_context_var
from restate.sql import SqlAttempt
from restate_agents import tool as restate_tool


class Teller(BaseChatModel):
    """
    Moves money as it is told to: "move 5 from 1 to 2" calls the tool "transfer";
    "refuse <reason>" calls "refuse", "remember <fact>" calls "remember";
    "use <tool> <json> then <tool> <json>" calls those tools in turn, with
    those inputs, and answers with their results, joined by " / "; a message
    steered to the turn (its id ``<turn>:steer:<n>``) it answers "Steered:
    <text>"; it echoes the rest.
    """

    calls: int = 0
    systems: list[str] = []

    @property
    def _llm_type(self) -> str:
        return "teller"

    def bind_tools(self, tools: Sequence[Any], **kwargs: Any) -> Teller:  # type: ignore[override]
        return self

    def _answer(self, messages: list[BaseMessage]) -> AIMessage:
        self.calls += 1
        self.systems.extend(str(m.content) for m in messages if isinstance(m, SystemMessage))
        last = messages[-1]
        if isinstance(last, HumanMessage) and ":steer:" in (last.id or ""):
            return AIMessage(content=f"Steered: {last.content}")
        steered = lambda m: ":steer:" in (m.id or "")  # noqa: E731
        asked = next(
            i
            for i in reversed(range(len(messages)))
            if isinstance(messages[i], HumanMessage) and not steered(messages[i])
        )
        if match := re.fullmatch(r"use (.+)", str(messages[asked].content)):
            steps = [step.split(" ", 1) for step in match.group(1).split(" then ")]
            results = [str(m.content) for m in messages[asked:] if isinstance(m, ToolMessage)]
            if len(results) == len(steps):
                return AIMessage(content=" / ".join(results))
            tool, args = steps[len(results)]
            return AIMessage(content="", tool_calls=[{"name": tool, "args": json.loads(args), "id": uuid.uuid4().hex}])
        if isinstance(last, ToolMessage):
            return AIMessage(content=f"Done: {last.content}")
        text = str(last.content)
        if match := re.fullmatch(r"move (\d+) from (\d+) to (\d+)", text):
            amount, src, dst = (int(g) for g in match.groups())
            call = {"name": "transfer", "args": {"src": src, "dst": dst, "amount": amount}, "id": uuid.uuid4().hex}
            return AIMessage(content="", tool_calls=[call])
        for name, arg in (("refuse", "reason"), ("remember", "fact")):
            if match := re.fullmatch(rf"{name} (.+)", text):
                call = {"name": name, "args": {arg: match.group(1)}, "id": uuid.uuid4().hex}
                return AIMessage(content="", tool_calls=[call])
        return AIMessage(content=f"You said: {text}")

    def _generate(
        self,
        messages: list[BaseMessage],
        stop: list[str] | None = None,
        run_manager: CallbackManagerForLLMRun | None = None,
        **kwargs: Any,
    ) -> ChatResult:
        return ChatResult(generations=[ChatGeneration(message=self._answer(messages))])

    async def _agenerate(
        self,
        messages: list[BaseMessage],
        stop: list[str] | None = None,
        run_manager: AsyncCallbackManagerForLLMRun | None = None,
        **kwargs: Any,
    ) -> ChatResult:
        return self._generate(messages, stop)


def pydantic_ai_model(teller: Teller) -> FunctionModel:
    """The teller, as a Pydantic AI model: it reads the conversation as LangChain messages."""

    def answer(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        said: list[BaseMessage] = [SystemMessage(info.instructions)] if info.instructions else []
        for m in messages:
            if isinstance(m, ModelResponse):
                text = "".join(p.content for p in m.parts if isinstance(p, TextPart))
                calls = [{"name": c.tool_name, "args": c.args_as_dict(), "id": c.tool_call_id} for c in m.tool_calls]
                said.append(AIMessage(content=text, tool_calls=calls))
                continue
            # A message steered to the turn comes after the tool results, in the same request.
            results = False
            for p in m.parts:
                if isinstance(p, UserPromptPart):
                    said.append(HumanMessage(str(p.content), id="turn:steer:" if results else None))
                elif isinstance(p, ToolReturnPart | RetryPromptPart):
                    said.append(ToolMessage(p.model_response_str(), tool_call_id=p.tool_call_id))
                    results = True
        reply = teller._answer(said)
        calls = [ToolCallPart(c["name"], c["args"], tool_call_id=c["id"]) for c in reply.tool_calls]
        return ModelResponse(parts=[*([TextPart(str(reply.content))] if reply.content else []), *calls])

    return FunctionModel(answer)


class ClaudeClient:
    """The teller, as an Anthropic client: ``client.beta.messages.create`` reads the messages as LangChain's."""

    def __init__(self, teller: Teller) -> None:
        self.teller = teller
        self.beta = self.messages = self
        self.requests: list[dict[str, Any]] = []

    async def create(self, *, messages: list[dict[str, Any]], system: str | None = None, **params: Any) -> Message:
        self.requests.append({"messages": list(messages), "system": system, **params})
        said: list[BaseMessage] = [SystemMessage(system)] if system else []
        for m in messages:
            content = [{"type": "text", "text": m["content"]}] if isinstance(m["content"], str) else m["content"]
            if m["role"] == "assistant":
                text = "".join(b["text"] for b in content if b["type"] == "text")
                calls = [
                    {"name": b["name"], "args": b["input"], "id": b["id"]} for b in content if b["type"] == "tool_use"
                ]
                said.append(AIMessage(content=text, tool_calls=calls))
                continue
            # A message steered to the turn comes after the tool results, in the same message.
            results = False
            for b in content:
                if b["type"] == "tool_result":
                    said.append(ToolMessage(b["content"], tool_call_id=b["tool_use_id"]))
                    results = True
                elif b["type"] == "text":
                    said.append(HumanMessage(b["text"], id="turn:steer:" if results else None))
        reply = self.teller._answer(said)
        calls = [ToolUseBlock(type="tool_use", id=c["id"], name=c["name"], input=c["args"]) for c in reply.tool_calls]
        text = [TextBlock(type="text", text=str(reply.content))] if reply.content else []
        return Message(
            id=uuid.uuid4().hex,
            type="message",
            role="assistant",
            model=params["model"],
            content=[*text, *calls],
            stop_reason="tool_use" if calls else "end_turn",
            usage=Usage(input_tokens=0, output_tokens=0),
        )


class Bank:
    """Accounts, and a transfer tool that moves money once per tool call id, as an idempotent service would."""

    def __init__(self) -> None:
        self.moves: dict[str, tuple[int, int, int]] = {}
        self.calls = 0
        # Called in the next tool call, after the move: such as Object.crash.
        self.crash_in_call: Callable[[], None] | None = None
        # Whether the process calling is alive: a dead one moves nothing.
        self.alive: Callable[[], bool] = lambda: True

    def tool(self):
        bank = self

        @tool
        async def transfer(src: int, dst: int, amount: int, tool_call_id: Annotated[str, InjectedToolCallId]) -> str:
            """Move money between two accounts."""
            if not bank.alive():
                raise ConnectionError("the process died")
            bank.calls += 1
            bank.moves.setdefault(tool_call_id, (src, dst, amount))
            if bank.crash_in_call is not None:
                crash, bank.crash_in_call = bank.crash_in_call, None
                crash()
                raise ConnectionError("the process died")
            return f"moved {amount} from {src} to {dst}"

        return transfer

    def restate_tool(self):
        """The transfer tool as a ``restate_agents.tool``, for any framework."""
        bank = self

        @restate_tool
        async def transfer(call_id: str, src: int, dst: int, amount: int) -> str:
            """Move money between two accounts."""
            if not bank.alive():
                raise ConnectionError("the process died")
            bank.calls += 1
            bank.moves.setdefault(call_id, (src, dst, amount))
            if bank.crash_in_call is not None:
                crash, bank.crash_in_call = bank.crash_in_call, None
                crash()
                raise ConnectionError("the process died")
            return f"moved {amount} from {src} to {dst}"

        return transfer


class Crash(Exception):
    """The process died: the attempt ended there, its later commands lost."""


@dataclass
class Request:
    id: str


class Object:
    """
    An object's state, over attempts: reads see the stored state and the
    attempt's writes; writes are stored in order, up to a crash.
    """

    _names = itertools.count()

    def __init__(self, key: str = "alice") -> None:
        self.key = key
        self.state: dict[str, bytes] = {}
        self.requests = 0
        # The state request at which the process dies, if any.
        self.crash_at: int | None = None
        # The process has died: the attempt's later requests fail, its writes are lost.
        self.crashed = False
        # One namespace per attempt: its process starts cold, as after a crash.
        self.name = f"agent-{next(self._names)}"
        # The object's other state (such as the turn it answers), the sends it made, and the messages steered to it.
        self.values: dict[str, Any] = {}
        self.sends: list[tuple[str, Any]] = []
        self.steered: list[dict[str, str]] = []
        # How often the agents published a change of the conversation (for its watches).
        self.published = 0
        # The calls made (service, handler, key, idempotency key, request), and how a call answers its request.
        self.calls: list[tuple[str, str, str | None, str | None, bytes]] = []
        self.answer: Callable[[str, bytes], bytes] = lambda handler, request: request
        # Called in the next call, after it was made: such as Object.crash.
        self.crash_in_call: Callable[[], None] | None = None

    def steer(self, text: str) -> None:
        """Steer the turn being answered with ``text``, as the agent's ``steer`` handler does."""
        self.steered.append({"id": f"turn:steer:{len(self.steered) + 1}", "text": text})

    def crash(self) -> None:
        """The process dies now."""
        self.crashed = True

    async def run(self, handler: Callable[..., Any], *args: Any, invocation: str | None = None) -> Any:
        """One attempt of ``handler``; raises Crash if it crashed, its state then stored up to the crash."""
        ctx = Context(self, invocation or uuid.uuid4().hex)
        token = _restate_context_var.set(ctx)  # type: ignore[arg-type]
        try:
            result = await handler(ctx, *args)
            await ctx.attempt.end(failed=False)
        except Exception as error:
            if self.crashed:
                self.crashed = False
                raise Crash() from error
            await ctx.attempt.end(failed=True)
            raise
        finally:
            _restate_context_var.reset(token)
        if self.crashed:
            self.crashed = False
            raise Crash()
        return result

    async def invoke(self, handler: Callable[..., Any], *args: Any, invocation: str | None = None) -> Any:
        """An invocation: its attempts until one completes, as Restate retries them."""
        invocation = invocation or uuid.uuid4().hex
        while True:
            try:
                return await self.run(handler, *args, invocation=invocation)
            except Crash:
                self.name = f"agent-{next(self._names)}"
                continue


class Context:
    """What the agents use of an object's context: its key, the request's id, and ``ctx.db``."""

    def __init__(self, obj: Object, invocation: str) -> None:
        self.obj = obj
        self.invocation = invocation
        self.attempt = SqlAttempt(self)  # type: ignore[arg-type]
        self.writes: list[tuple[str, bytes | None]] = []

    def key(self) -> str:
        return self.obj.key

    def request(self) -> Request:
        return Request(self.invocation)

    @property
    def db(self):
        return self.attempt.db

    def _alive(self) -> bool:
        """Whether the process is still alive at this request."""
        self.obj.requests += 1
        if self.obj.crash_at is not None and self.obj.requests >= self.obj.crash_at:
            self.obj.crash_at = None
            self.obj.crash()
        return not self.obj.crashed

    # --- the object's state, calls and sends, as a turn uses them (besides ctx.db)

    async def get(self, key: str) -> Any:
        return self.obj.values.get(key)

    def set(self, key: str, value: Any) -> None:
        self.obj.values[key] = value

    def clear(self, key: str) -> None:
        self.obj.values.pop(key, None)

    def object_send(self, handler: Callable[..., Any], key: str, arg: Any, **options: Any) -> None:
        self.obj.sends.append((handler.__name__, arg))

    async def generic_call(
        self, service: str, handler: str, arg: bytes, key: str | None = None, idempotency_key: str | None = None
    ) -> bytes:
        self.obj.calls.append((service, handler, key, idempotency_key, arg))
        if self.obj.crash_in_call is not None:
            crash, self.obj.crash_in_call = self.obj.crash_in_call, None
            crash()
            raise ConnectionError("the process died")
        return self.obj.answer(handler, arg)

    async def object_call(self, handler: Callable[..., Any], key: str, arg: Any, **options: Any) -> Any:
        assert handler.__name__ in ("steering", "publish"), f"the agents call {handler.__name__} here"
        if handler.__name__ == "publish":
            self.obj.published += 1
            return {"revision": self.obj.published}
        return list(self.obj.steered)

    # --- the SqlHost of restate.sql

    sql_handler_kind = "exclusive"
    sql_journal_replay = False

    @property
    def sql_namespace(self) -> str:
        return self.obj.name

    def sql_get(self, key: str):
        future = asyncio.get_running_loop().create_future()
        if self._alive():
            future.set_result(self.obj.state.get(key))
        else:
            future.set_exception(ConnectionError("the process died"))
        return future

    def sql_set(self, key: str, value: bytes) -> None:
        if self._alive():
            self.obj.state[key] = bytes(value)

    def sql_clear(self, key: str) -> None:
        if self._alive():
            self.obj.state.pop(key, None)

    async def sql_sync(self) -> None:
        if not self._alive():
            raise ConnectionError("the process died")

    def sql_send(self, *args: Any) -> None:
        raise AssertionError("the agents make no sends here")

    def sql_attempt_end(self) -> BaseException | None:
        return None

    def sql_on_attempt_end(self, listener: Callable[[], None]) -> Callable[[], None]:
        return lambda: None

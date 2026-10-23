"""What the integrations share: the chat handler's message, the model, and the conversation as ``messages`` gives it."""

from __future__ import annotations

import functools
import inspect
import json
import os
from dataclasses import dataclass, field
from typing import Any, Literal, NotRequired, TypedDict

from restate.serde import Serde
from restate_fns.schema import Schema


class MessageSerde(Serde[str]):
    """A chat message: the request's body as text, or the string it is as JSON (``"Hi"``)."""

    def deserialize(self, buf: bytes) -> str | None:
        text = buf.decode()
        try:
            value = json.loads(text)
        except ValueError:
            return text
        return value if isinstance(value, str) else text

    def serialize(self, obj: str | None) -> bytes:
        return json.dumps(obj).encode()


@functools.cache
def table_schema(table: str, create: str) -> Schema:
    """
    The schema of an integration's table ``table`` (``restate_<integration>_...``,
    apart from the tables of yours), named after it: one step, ``create``,
    which ``ensure`` applies where the table is used (in any object; in a
    shared handler, in its own session, so an empty table reads as empty).
    """
    return Schema(table, [create])


DEFAULT_MODEL = "anthropic/claude-sonnet-5-5"
"""The model of an agent given none, and no MODEL."""


async def configured(option: Any, ctx: Any, value: type | tuple[type, ...] = ()) -> Any:
    """
    ``option`` for the invocation of ``ctx``: the option itself, or what a
    function of the context gives (awaited, if it is a coroutine), the same
    for every message or each user's. ``value``: the types that are an option
    themselves, though callable (a framework's model, say).
    """
    if not callable(option) or isinstance(option, value):
        return option
    given = option(ctx)
    return await given if inspect.isawaitable(given) else given


class ModelError(ValueError):
    """An agent's model that it cannot take: not ``provider/model-id``, or another provider's."""


def model_of(model: str | None) -> str:
    """The model, as ``provider/model-id``: ``model``, or else MODEL, or else ``DEFAULT_MODEL``."""
    return model or os.environ.get("MODEL") or DEFAULT_MODEL


def model_id_of(model: str, provider: str) -> str:
    """The model id of ``model``, for a framework with only ``provider``'s models: ``provider/model-id``, or the id."""
    named, id = model.split("/", 1) if "/" in model else (provider, model)
    if named != provider or not id:
        source = " (from MODEL)" if os.environ.get("MODEL") == model else ""
        raise ModelError(f'the model "{model}"{source} is not {provider}\'s: this agent takes {provider}/<model-id>')
    return id


CHECK_MODEL = "check the agent's model (its `model` option, or MODEL) and its provider's API key"

# The model providers' errors of a request that fails again however often it is made.
_FOR_GOOD = {"AuthenticationError", "PermissionDeniedError", "NotFoundError", "BadRequestError"}


def refused(error: BaseException) -> bool:
    """
    Whether ``error`` is a model provider's refusal that a retry does not
    change: a missing or wrong API key, an unknown model, a bad request.
    Other errors (a tool's, an overloaded provider's) fail the attempt, and
    Restate retries it.
    """
    if isinstance(error, ModelError):
        return True
    if any(kind.__name__ in _FOR_GOOD for kind in type(error).__mro__):
        return True
    if type(error).__name__ == "ModelHTTPError":  # Pydantic AI's, of any provider
        return getattr(error, "status_code", None) in (400, 401, 403, 404)
    text = str(error).lower()
    return any(sign in text for sign in ("api key", "api_key", "could not resolve authentication"))


class TextPart(TypedDict):
    """A message's text."""

    type: Literal["text"]
    text: str


class ToolPart(TypedDict):
    """A tool call of the assistant's message, with its output once there is one."""

    type: Literal["tool"]
    id: str
    name: str
    input: Any
    output: NotRequired[str]
    error: NotRequired[bool]
    """Whether ``output`` is the tool's error, for a UI to show it as one."""


class ChatMessage(TypedDict):
    """A message of a conversation, as an agent's ``messages`` handler returns it."""

    id: int
    """Its id, from 0, in the order of the conversation; the same however it changes. ``before`` takes it."""
    role: Literal["user", "assistant"]
    parts: list[TextPart | ToolPart]


class MessagesRequest(TypedDict, total=False):
    """
    Which messages to read: the latest ``limit`` (default 50), or those before
    a message (its ``id``), for a page of the conversation; or those new or
    changed since a ``cursor``, to follow it.
    """

    limit: int
    before: int
    cursor: int


class Messages(TypedDict):
    """
    A page of messages: oldest first, and whether there are older ones; or,
    with a ``cursor``, those new or changed since it, by when they changed,
    and whether there are more of them. Keep them by ``id``: a message comes
    again once it changed (its tool call's output came, say). ``cursor``: the
    cursor to read on from.
    """

    messages: list[ChatMessage]
    more: bool
    cursor: int


@dataclass
class Transcript:
    """
    A conversation as ``messages`` gives it, built from a framework's
    transcript: each message is started by ``say``, a tool call goes to the
    assistant's message, and its result to its call. Messages without parts
    are left out.
    """

    _messages: list[dict[str, Any]] = field(default_factory=list)
    _calls: dict[str, ToolPart] = field(default_factory=dict)

    def say(self, role: Literal["user", "assistant"], text: str = "") -> None:
        """A new message of ``role``, with ``text`` if there is any."""
        self._messages.append({"role": role, "parts": [TextPart(type="text", text=text)] if text.strip() else []})

    def call(self, id: str, name: str, input: Any) -> None:
        """Tool call ``id`` of the assistant's message."""
        if not self._messages or self._messages[-1]["role"] != "assistant":
            self.say("assistant")
        part = ToolPart(type="tool", id=id, name=name, input=input)
        self._messages[-1]["parts"].append(part)
        self._calls[id] = part

    def result(self, id: str, output: str, error: bool = False) -> None:
        """The output of tool call ``id``; with ``error``, the tool's error."""
        if id in self._calls:
            self._calls[id]["output"] = output
            if error:
                self._calls[id]["error"] = True

    def messages(self) -> list[dict[str, Any]]:
        """Its messages, in order (each ``{role, parts}``): those with parts."""
        return [{"role": m["role"], "parts": m["parts"]} for m in self._messages if m["parts"]]


class SyncRequest(TypedDict, total=False):
    """
    What ``sync`` reads: what is new or changed since ``cursor`` (default 0,
    all of it), at most ``limit`` messages (default 100); once the revision is
    past ``revision``, if given, or at ``timeoutSeconds``.
    """

    cursor: int
    revision: int
    timeoutSeconds: float
    limit: int


class Synced(Messages):
    """What ``sync`` answers: the messages since the cursor, as ``messages`` gives them, the status and the revision."""

    revision: int
    status: dict[str, Any]

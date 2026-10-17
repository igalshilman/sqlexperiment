"""What the integrations share: the chat handler's message, the model, and the conversation as ``messages`` gives it."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from typing import Any, Literal, NotRequired, TypedDict

from restate.serde import Serde


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


def model_of(model: str | None, fallback: str) -> str:
    """The model, as ``provider/model-id``: ``model``, or else MODEL, or else the framework's default."""
    return model or os.environ.get("MODEL") or fallback


def model_id_of(model: str, provider: str) -> str:
    """The model id of ``model``, for a framework with only ``provider``'s models: ``provider/model-id``, or the id."""
    named, id = model.split("/", 1) if "/" in model else (provider, model)
    if named != provider or not id:
        source = " (from MODEL)" if os.environ.get("MODEL") == model else ""
        raise ValueError(f'the model "{model}"{source} is not {provider}\'s: this agent takes {provider}/<model-id>')
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
    if type(error).__name__ in _FOR_GOOD:
        return True
    if type(error).__name__ == "ModelHTTPError":  # Pydantic AI's, of any provider
        return getattr(error, "status_code", None) in (400, 401, 403, 404)
    text = str(error).lower()
    return any(sign in text for sign in ("api key", "api_key", "could not resolve authentication"))


class TextPart(TypedDict):
    type: Literal["text"]
    text: str


class ToolPart(TypedDict):
    type: Literal["tool"]
    id: str
    name: str
    input: Any
    output: NotRequired[str]


class ChatMessage(TypedDict):
    """A message of a conversation, as an agent's ``messages`` handler returns it."""

    id: int
    """Its position in the conversation, from 0; ``before`` and ``after`` take it."""
    role: Literal["user", "assistant"]
    parts: list[TextPart | ToolPart]


class MessagesRequest(TypedDict, total=False):
    """Which messages to read: the latest ``limit`` (default 50), or those before or after a message (its ``id``)."""

    limit: int
    before: int
    after: int


class Messages(TypedDict):
    """A page of messages, oldest first, and whether there are more (older ones, or with ``after``, newer ones)."""

    messages: list[ChatMessage]
    more: bool


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

    def result(self, id: str, output: str) -> None:
        """The output of tool call ``id``."""
        if id in self._calls:
            self._calls[id]["output"] = output

    def page(self, request: MessagesRequest | None = None) -> Messages:
        """The page of ``request``."""
        request = request or {}
        limit = request.get("limit", 50)
        all = [
            ChatMessage(id=id, role=m["role"], parts=m["parts"])
            for id, m in enumerate(m for m in self._messages if m["parts"])
        ]
        if "after" in request:
            newer = all[request["after"] + 1 :]
            return Messages(messages=newer[:limit], more=len(newer) > limit)
        end = len(all) if "before" not in request else max(0, min(request["before"], len(all)))
        start = max(0, end - limit)
        return Messages(messages=all[start:end], more=start > 0)

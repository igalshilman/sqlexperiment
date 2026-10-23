"""
An agent's conversation, from any Python code (a web app's backend, a
script, a notebook), through Restate's ingress: send it messages, read what
it said and is doing, approve what it waits for, steer or stop it, and
``follow`` it as it changes. It needs nothing of the agent's code: only
Restate's URL, and the agent's name and key::

    from restate_agents.client import AgentClient

    async with AgentClient("http://localhost:8080", "support", "alice") as alice:
        print(await alice.chat("Order 103 arrived broken, can I get my $30 back?"))
        async for change in alice.follow():
            for message in change.messages:
                print(message)
"""

from __future__ import annotations

import asyncio
import json
import logging
import uuid
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import timedelta
from typing import Any

import httpx

from restate_agents.common import ChatMessage, Messages, Synced

__all__ = ["AgentClient", "AgentError", "Followed", "as_text"]

log = logging.getLogger(__name__)


class AgentError(Exception):
    """A request the agent answered with an error: its HTTP status, and Restate's message."""

    def __init__(self, status: int, message: str) -> None:
        super().__init__(f"{status}: {message}")
        self.status = status
        self.message = message


@dataclass
class Followed:
    """What ``follow`` gives each time the conversation changes."""

    status: dict[str, Any]
    """What the agent is doing now: ``{"answering": ..., "waiting": [...]}``."""
    messages: list[ChatMessage]
    """
    The messages new or changed since the last time, by when they changed: a
    message may come again, changed (a tool call with its output, say), with
    its ``id``, which a UI keeps them by, and orders them by.
    """
    cursor: int
    """Where this leaves off: ``follow(cursor=...)`` carries on from here, later or elsewhere."""

    @property
    def waiting(self) -> list[dict[str, Any]]:
        """What the agent's tools wait for now (``pending``): each wait's ``id``, ``name`` and ``input``."""
        return self.status["waiting"]


def as_text(message: ChatMessage) -> str:
    """
    ``message`` to read, such as in a terminal: its text after its role, and
    each tool call on a line of its own, with its output (``…`` until it has one).
    """
    lines = []
    for part in message["parts"]:
        if part["type"] == "text":
            lines.append(f"{message['role']}: {part['text']}")
        else:
            lines.append(f"  {part['name']}({json.dumps(part['input'])}) -> {part.get('output', '…')}")
    return "\n".join(lines)


class AgentClient:
    """The conversation of the agent ``agent`` (an object's name) with key ``key``, at Restate's ingress ``url``."""

    def __init__(self, url: str, agent: str, key: str, *, http: httpx.AsyncClient | None = None) -> None:
        self.url = f"{url.rstrip('/')}/{agent}/{key}"
        self.ingress = url.rstrip("/")
        self._http = http or httpx.AsyncClient()
        self._own = http is None

    async def __aenter__(self) -> AgentClient:
        return self

    async def __aexit__(self, *_: Any) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        if self._own:
            await self._http.aclose()

    async def chat(self, message: str, *, idempotency_key: str | None = None) -> str:
        """The agent's answer to ``message``. With an idempotency key, sending it again is the same message."""
        return await self.call("chat", message, idempotency_key=idempotency_key, timeout=None)

    async def send(self, message: str, *, idempotency_key: str | None = None, delay: timedelta | None = None) -> str:
        """Send ``message`` without waiting for the answer (after ``delay``, if given): its invocation's id."""
        query = f"?delay={int(delay.total_seconds() * 1000)}ms" if delay else ""
        sent = await self.call(f"chat/send{query}", message, idempotency_key=idempotency_key)
        return sent["invocationId"]

    async def answer(self, invocation_id: str) -> str:
        """The answer to the message sent as ``invocation_id``, once there is one."""
        return await self._request("GET", f"{self.ingress}/restate/invocation/{invocation_id}/attach", timeout=None)

    async def messages(self, *, limit: int = 50, before: int | None = None, cursor: int | None = None) -> Messages:
        """
        A page of the conversation: the latest ``limit`` messages, or those
        ``before`` an ``id``; or those new or changed since ``cursor``. Each
        page has the cursor to read on from.
        """
        request: dict[str, int] = {"limit": limit}
        if before is not None:
            request["before"] = before
        if cursor is not None:
            request["cursor"] = cursor
        return await self.call("messages", request)

    async def sync(
        self,
        *,
        cursor: int = 0,
        revision: int | None = None,
        timeout_seconds: float = 50,
        limit: int = 100,
        idempotency_key: str | None = None,
    ) -> Synced:
        """
        The messages new or changed since ``cursor`` (at most ``limit``), what
        the agent is doing, and the revision, in one request; with
        ``revision``, once there is a newer one, or at the timeout.
        """
        request: dict[str, Any] = {"cursor": cursor, "timeoutSeconds": timeout_seconds, "limit": limit}
        if revision is not None:
            request["revision"] = revision
        return await self.call("sync", request, idempotency_key=idempotency_key, timeout=timeout_seconds + 30)

    async def status(self) -> dict[str, Any]:
        """What the agent is doing: the message it answers, if any, and what its tools wait for."""
        return await self.call("status")

    async def stop(self) -> str | None:
        """Stop the message the agent answers: the invocation stopped, or None if it answered none."""
        return (await self.call("stop"))["stopped"]

    async def steer(self, message: str) -> dict[str, Any]:
        """Add ``message`` to the message the agent answers, at its next tool call (or send it as one of its own)."""
        return await self.call("steer", message)

    async def pending(self) -> list[dict[str, Any]]:
        """What the agent's tools wait for, oldest first."""
        return await self.call("pending")

    async def signal(
        self, value: Any, *, id: str | None = None, name: str | None = None, keep: bool = False
    ) -> str | None:
        """Resolve the wait ``id``, or the oldest for the signal ``name``, with ``value``: the wait it resolved."""
        signal = {"id": id, "name": name, "value": value, "keep": keep}
        return (await self.call("signal", signal))["resolved"]

    async def notifications(self) -> int:
        """The revision of the conversation, which each change increases."""
        return (await self.call("notifications"))["revision"]

    async def watch(self, revision: int, *, timeout_seconds: float = 50, idempotency_key: str | None = None) -> int:
        """The revision, once there is one newer than ``revision``, or at the timeout."""
        watch = {"revision": revision, "timeoutSeconds": timeout_seconds}
        changed = await self.call("watch", watch, idempotency_key=idempotency_key, timeout=timeout_seconds + 30)
        return changed["revision"]

    async def follow(self, *, cursor: int = 0, timeout_seconds: float = 50) -> AsyncIterator[Followed]:
        """
        The conversation as it changes: what the agent is doing and the
        messages since ``cursor`` (by default, all of them), then each change,
        for as long as you read it.

        Each change is one ``sync``, which waits in Restate for the next. A
        network failure (or Restate's, restarting) does not end it: it tries
        again, and a sync it retries keeps its idempotency key, so it joins
        the one already waiting.
        """
        revision: int | None = None
        while True:
            synced = await self._sync_retried(cursor, revision, timeout_seconds)
            # Nothing changed (the wait timed out): wait again.
            if synced["revision"] != revision or synced["messages"]:
                yield Followed(status=synced["status"], messages=synced["messages"], cursor=synced["cursor"])
            cursor = synced["cursor"]
            # With more to read, at once; else once something changes.
            revision = None if synced["more"] else synced["revision"]

    async def _sync_retried(self, cursor: int, revision: int | None, timeout_seconds: float) -> Synced:
        """One ``sync``, tried again with the same idempotency key until it answers, unless its error would last."""
        window = uuid.uuid4().hex
        while True:
            try:
                return await self.sync(
                    cursor=cursor, revision=revision, timeout_seconds=timeout_seconds, idempotency_key=window
                )
            except (httpx.TransportError, AgentError) as error:
                if not _transient(error):
                    raise
                log.warning("follow: retrying after %s", error)
                await asyncio.sleep(1)

    async def call(
        self, handler: str, body: Any = None, *, idempotency_key: str | None = None, timeout: float | None = 30
    ) -> Any:
        """
        The answer of the agent's handler ``handler``, such as one of your own
        (``@agent.handler``), to ``body`` as JSON: ``await alice.call("facts")``.
        """
        headers = {"idempotency-key": idempotency_key} if idempotency_key else {}
        return await self._request("POST", f"{self.url}/{handler}", body, headers, timeout)

    async def _request(
        self, method: str, url: str, body: Any = None, headers: dict[str, str] | None = None, timeout: float | None = 30
    ) -> Any:
        response = await self._http.request(
            method, url, json=body, headers=headers or {}, timeout=httpx.Timeout(timeout, connect=10)
        )
        if response.is_error:
            try:
                message = response.json().get("message", response.text)
            except ValueError:
                message = response.text
            raise AgentError(response.status_code, message)
        return response.json() if response.content else None


def _transient(error: Exception) -> bool:
    """Whether a retry may answer: a network failure, Restate's (5xx) or too many requests (429); not a 4xx."""
    return not isinstance(error, AgentError) or error.status >= 500 or error.status == 429

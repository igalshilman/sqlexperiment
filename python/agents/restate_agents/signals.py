"""
Signals to agents: a virtual object per agent, keyed ``<agent>:<key>`` (such
as ``support:alice``), whose ``wait`` returns once ``resolve`` gives the wait
its value; and the messages steered to the turn the agent is answering.

An agent's own handler cannot wait on an awakeable (it has no journal to
replay one from), so it calls ``wait``, which can: a journaled handler of
this object, which waits without holding a process.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import Any

import restate
from pydantic import BaseModel
from restate import ObjectContext, ObjectSharedContext, TerminalError

signals = restate.VirtualObject("signals")


def signals_of(service: str, key: str) -> str:
    """The key of the signals of the object ``service``'s key ``key``."""
    return f"{service}:{key}"


class Waiting(BaseModel):
    """A wait for a signal, as ``pending`` lists it."""

    id: str
    """The wait's id, which ``signal`` takes to resolve this wait and no other."""
    name: str
    """The signal it waits for, such as ``approval``."""
    input: Any = None
    """What it waits for, as the model said it (such as the refund to approve), if the tool takes an input."""
    since: str = ""
    """When it started, as an ISO date."""


class Signal(BaseModel):
    """Which wait a signal resolves, and with what."""

    id: str | None = None
    """The wait of this id (from ``pending``)."""
    name: str | None = None
    """Else the oldest wait for the signal of this name."""
    value: Any = None
    """Its value: what the wait answers the model with."""
    keep: bool = False
    """With a ``name`` and no one waiting for it, keep the signal for the next wait for it (else it is dropped)."""


class Resolved(BaseModel):
    """What ``signal`` did: the id of the wait it resolved, or None if none waited (and it was dropped, or kept)."""

    resolved: str | None


class Steered(BaseModel):
    """
    What ``steer`` did: the invocation that answers the message (to attach
    to), and whether it steers the message being answered, or else (with none
    answered) was sent as a message of its own.
    """

    invocationId: str  # noqa: N815 (as Restate names it)
    steered: bool


class Steer(BaseModel):
    """A message steered to a turn: its id (``<turn>:steer:<n>``), which the turn takes it by, and its text."""

    id: str
    text: str


class Agent(BaseModel):
    """An agent: its object's name, and key."""

    service: str
    key: str


class SteerRequest(BaseModel):
    agent: Agent
    turn: str | None
    text: str


class CloseRequest(BaseModel):
    agent: Agent
    turn: str
    taken: list[str]
    cancelled: bool


# The turns closed last, newest last: a message steered to one of them is a message of its own.
CLOSED = 16


async def _send(ctx: ObjectContext, agent: Agent, text: str) -> str:
    """Send ``text`` to ``agent``'s ``chat``, as a message of its own: its invocation's id."""
    handle = ctx.generic_send(agent.service, "chat", arg=json.dumps(text).encode(), key=agent.key)
    return await handle.invocation_id()


class _Listening(Waiting):
    awakeable: str


@signals.handler(kind="shared")
async def wait(ctx: ObjectSharedContext, request: Waiting) -> Any:
    """Wait for the signal ``request.name``, as wait ``request.id``, and return its value."""
    awakeable, value = ctx.awakeable()
    since = datetime.fromtimestamp(await ctx.time(), UTC).isoformat()
    listening = _Listening(**request.model_dump(exclude={"since"}), since=since, awakeable=awakeable)
    ctx.object_send(listen, key=ctx.key(), arg=listening)
    try:
        return await value
    except TerminalError as error:
        # The agent's invocation was cancelled: no one waits any more.
        if error.status_code == 409:
            ctx.object_send(forget, key=ctx.key(), arg=request.id)
        raise


@signals.handler()
async def resolve(ctx: ObjectContext, signal: Signal) -> Resolved:
    """Resolve the wait ``signal.id``, or else the oldest wait for ``signal.name``, with ``signal.value``."""
    if signal.id is None and signal.name is None:
        raise TerminalError("a signal needs an id or a name")
    waiting = await ctx.get("waiting") or []
    match = "id" if signal.id is not None else "name"
    at = next((i for i, w in enumerate(waiting) if w[match] == getattr(signal, match)), None)
    if at is None:
        if signal.keep and signal.name is not None and signal.id is None:
            kept = await ctx.get("kept") or {}
            ctx.set("kept", {**kept, signal.name: [*kept.get(signal.name, []), signal.value]})
        return Resolved(resolved=None)
    found = waiting.pop(at)
    ctx.resolve_awakeable(found["awakeable"], signal.value)
    ctx.set("waiting", waiting)
    return Resolved(resolved=found["id"])


@signals.handler(kind="shared")
async def pending(ctx: ObjectSharedContext) -> list[dict[str, Any]]:
    """The waits, oldest first, as ``Waiting``."""
    return [Waiting(**w).model_dump() for w in await ctx.get("waiting") or []]


@signals.handler(ingress_private=True)
async def listen(ctx: ObjectContext, request: _Listening) -> None:
    """A wait: resolved now with a signal kept for it, or else when one comes."""
    kept = await ctx.get("kept") or {}
    if values := kept.get(request.name):
        value, *rest = values
        ctx.resolve_awakeable(request.awakeable, value)
        ctx.set("kept", {**kept, request.name: rest})
        return
    ctx.set("waiting", [*(await ctx.get("waiting") or []), request.model_dump()])


@signals.handler(ingress_private=True)
async def steer(ctx: ObjectContext, request: SteerRequest) -> Steered:
    """
    Steer ``request.turn`` (the message ``request.agent`` answers, if any)
    with ``request.text``: queued for it to take at its next step, or, if it
    is over, sent as a message of its own.
    """
    turn = request.turn
    if turn is None or turn in (await ctx.get("closed") or []):
        return Steered(invocationId=await _send(ctx, request.agent, request.text), steered=False)
    steering = await ctx.get("steering")
    messages = steering["messages"] if steering and steering["turn"] == turn else []
    steer = Steer(id=f"{turn}:steer:{len(messages) + 1}", text=request.text)
    ctx.set("steering", {"turn": turn, "messages": [*messages, steer.model_dump()]})
    return Steered(invocationId=turn, steered=True)


@signals.handler(kind="shared", ingress_private=True)
async def steering(ctx: ObjectSharedContext, turn: str) -> list[dict[str, Any]]:
    """The messages steered to ``turn``, in order, as ``Steer``."""
    steering = await ctx.get("steering")
    return steering["messages"] if steering and steering["turn"] == turn else []


@signals.handler(ingress_private=True)
async def close(ctx: ObjectContext, request: CloseRequest) -> None:
    """
    ``request.turn`` is over: each message steered to it that it did not take
    (not in ``request.taken``) is sent as a message of its own, unless the
    turn was cancelled; later ones are, too.
    """
    closed = await ctx.get("closed") or []
    if request.turn in closed:
        return
    ctx.set("closed", [*closed, request.turn][-CLOSED:])
    steering = await ctx.get("steering")
    if not steering or steering["turn"] != request.turn:
        return
    ctx.clear("steering")
    if request.cancelled:
        return
    for message in steering["messages"]:
        if message["id"] not in request.taken:
            await _send(ctx, request.agent, message["text"])


@signals.handler(ingress_private=True)
async def forget(ctx: ObjectContext, id: str) -> None:
    """Forget the wait ``id``, whose agent no longer waits."""
    ctx.set("waiting", [w for w in await ctx.get("waiting") or [] if w["id"] != id])

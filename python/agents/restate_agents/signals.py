"""
Signals to agents: a virtual object per agent, keyed ``<agent>:<key>`` (such
as ``support:alice``), whose ``wait`` returns once ``resolve`` gives the wait
its value; the messages steered to the turn the agent is answering; and the
revision of the agent's conversation, which its ``watch`` waits on.

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


class Notifications(BaseModel):
    """The revision of an agent's conversation: each change of what a client reads increases it."""

    revision: int


class Watch(BaseModel):
    """What ``watch`` waits for: a revision newer than ``revision``, for at most ``timeoutSeconds`` (at most 60)."""

    revision: int = 0
    timeoutSeconds: float = 50  # noqa: N815


class Subscription(BaseModel):
    """A ``watch`` waiting: the awakeable to resolve once there is a revision newer than ``revision``."""

    revision: int
    awakeable: str


class Agent(BaseModel):
    """An agent: its object's name, and key."""

    service: str
    key: str


class SteerRequest(BaseModel):
    """A message steered to ``agent``'s turn ``turn`` (or, with none, sent as a message of its own)."""

    agent: Agent
    turn: str | None
    text: str


class CloseRequest(BaseModel):
    """The end of ``agent``'s turn ``turn``: the steered messages it took, and whether it was cancelled."""

    agent: Agent
    turn: str
    taken: list[str]
    cancelled: bool


# The turns closed last, newest last: a message steered to one of them is a message of its own.
CLOSED = 16


async def _changed(ctx: ObjectContext) -> Notifications:
    """The conversation changed: increase its revision, and answer each ``watch`` waiting for it."""
    changed = Notifications(revision=(await ctx.get("revision") or 0) + 1)
    ctx.set("revision", changed.revision)
    for watching in await ctx.get("watching") or []:
        ctx.resolve_awakeable(watching["awakeable"], changed.model_dump())
    ctx.clear("watching")
    return changed


async def _send(ctx: ObjectContext, agent: Agent, text: str) -> str:
    """Send ``text`` to ``agent``'s ``chat``, as a message of its own: its invocation's id."""
    handle = ctx.generic_send(agent.service, "chat", arg=json.dumps(text).encode(), key=agent.key)
    return await handle.invocation_id()


class _Listening(Waiting):
    """A wait, with the awakeable that resolves it."""

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
    await _changed(ctx)
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
    await _changed(ctx)


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
    await _changed(ctx)
    return Steered(invocationId=turn, steered=True)


@signals.handler(kind="shared", ingress_private=True)
async def steering(ctx: ObjectSharedContext, turn: str) -> list[dict[str, Any]]:
    """The messages steered to ``turn``, in order, as ``Steer``."""
    steering = await ctx.get("steering")
    return steering["messages"] if steering and steering["turn"] == turn else []


@signals.handler(ingress_private=True)
async def publish(ctx: ObjectContext) -> Notifications:
    """The agent's conversation changed (a message stored, say): its revision increases."""
    return await _changed(ctx)


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
    await _changed(ctx)
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
async def reset(ctx: ObjectContext) -> None:
    """The agent forgot its conversation (``clear``): forget its waits, kept signals and steered messages."""
    for key in ("waiting", "kept", "steering"):
        ctx.clear(key)
    await _changed(ctx)


@signals.handler(ingress_private=True)
async def forget(ctx: ObjectContext, id: str) -> None:
    """Forget the wait ``id``, whose agent no longer waits."""
    ctx.set("waiting", [w for w in await ctx.get("waiting") or [] if w["id"] != id])
    await _changed(ctx)


@signals.handler(kind="shared")
async def notifications(ctx: ObjectSharedContext) -> Notifications:
    """The conversation's revision."""
    return Notifications(revision=await ctx.get("revision") or 0)


@signals.handler(ingress_private=True)
async def subscribe(ctx: ObjectContext, subscription: Subscription) -> None:
    """Answer ``subscription.awakeable`` once there is a revision newer than ``revision``: at once, if there is."""
    revision = await ctx.get("revision") or 0
    if revision > subscription.revision:
        ctx.resolve_awakeable(subscription.awakeable, Notifications(revision=revision).model_dump())
        return
    ctx.set("watching", [*(await ctx.get("watching") or []), subscription.model_dump()])


@signals.handler(ingress_private=True)
async def unsubscribe(ctx: ObjectContext, awakeable: str) -> None:
    """Forget the ``watch`` of ``awakeable``, which no longer waits."""
    ctx.set("watching", [w for w in await ctx.get("watching") or [] if w["awakeable"] != awakeable])

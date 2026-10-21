"""
A turn: the message an agent's ``chat`` answers. While it answers, the
object's ``status`` says so; ``stop`` cancels it; and ``steer`` adds
messages to it, which a framework gives its conversation at its next tool
call (``steering``). Those it did not take when it ends are answered as
messages of their own, unless it was cancelled.
"""

from __future__ import annotations

import weakref
from collections.abc import Awaitable, Callable, Iterable
from contextvars import ContextVar
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

import restate_fns as restate

from restate_agents.common import CHECK_MODEL, MessageSerde, refused
from restate_agents.object import TURN, Object, Turn, context, service
from restate_agents.signals import Agent, CloseRequest, Steer, signals_of
from restate_agents.signals import close as close_turn
from restate_agents.signals import publish as publish_change
from restate_agents.signals import steering as steering_of

# A cancelled turn's answer, as its conversation records it.
CANCELLED = "Cancelled: the turn was stopped before it finished."


@dataclass
class _Answering:
    """The turn this attempt answers: its invocation's id, and the ids of the steered messages its conversation has."""

    id: str
    taken: set[str] = field(default_factory=set)


_answering: ContextVar[_Answering | None] = ContextVar("restate_agents.answering", default=None)

# The prompts made in each attempt, by its context.
_prompts: weakref.WeakKeyDictionary[Any, int] = weakref.WeakKeyDictionary()


def prompt_id(ctx: Any) -> str:
    """The id of the attempt's next prompt: the invocation's id for its first, then ``<id>:2``, and so on."""
    count = _prompts[ctx] = _prompts.get(ctx, 0) + 1
    id = ctx.request().id
    return id if count == 1 else f"{id}:{count}"


async def steering(ctx: Any) -> list[Steer]:
    """
    The messages steered to the turn being answered that its conversation
    does not have yet, in order; none outside a ``chat``. A framework gives
    them to its conversation at the end of a tool call (by their ids, which
    it keeps), and says so with ``taken``.
    """
    turn = _answering.get()
    if turn is None:
        return []
    messages = await ctx.object_call(steering_of, key=signals_of(service(), ctx.key()), arg=turn.id)
    return [Steer(**m) for m in messages if m["id"] not in turn.taken]


async def publish() -> None:
    """
    The conversation changed (a framework stored a message, say): the clients
    that follow it (``watch``) read it again. It is a call, so what the
    handler stored before is stored before they read it. Outside an agent's
    handler (a checkpointer's test, say), it does nothing.
    """
    try:
        ctx, name = context(), service()
    except RuntimeError:
        return
    await ctx.object_call(publish_change, key=signals_of(name, ctx.key()), arg=None)


def taken(ids: Iterable[str]) -> None:
    """The conversation has the steered messages ``ids``: the turn does not answer them as messages of their own."""
    if (turn := _answering.get()) is not None:
        turn.taken.update(ids)


def chat(obj: Object, answer: Callable[[Any, str], Awaitable[str]]) -> None:
    """
    ``answer`` as ``obj``'s ``chat`` handler, which takes its message as plain
    text or as a JSON string (``curl -d 'Hi'``, ``curl --json '"Hi"'``).

    While it answers, ``status`` says so (its ``Turn``). Once it ends, the
    messages steered to it that it did not take are answered as messages of
    their own, unless it was cancelled. A model provider's refusal (a missing
    API key, an unknown model) is a ``restate.TerminalError``.
    """

    async def chat(ctx: restate.ObjectContext, message: str) -> str:
        id = ctx.request().id
        turn = await ctx.get(TURN)
        if not turn or turn["invocationId"] != id:
            ctx.set(TURN, Turn(invocationId=id, since=datetime.now(UTC).isoformat()).model_dump())
        answering = _Answering(id)
        _answering.set(answering)
        await publish()

        def end(cancelled: bool = False) -> None:
            ctx.clear(TURN)
            agent = Agent(service=obj.name, key=ctx.key())
            closing = CloseRequest(agent=agent, turn=id, taken=sorted(answering.taken), cancelled=cancelled)
            ctx.object_send(close_turn, key=signals_of(obj.name, ctx.key()), arg=closing)

        try:
            answered = await answer(ctx, message)
        except restate.TerminalError as error:
            end(cancelled=error.status_code == 409)
            raise
        except Exception as error:
            # Any other error fails the attempt, and Restate retries the turn.
            if not refused(error):
                raise
            end()
            raise restate.TerminalError(f"the model's provider refused the request: {CHECK_MODEL}. {error}") from error
        end()
        return answered

    obj.handler("chat", accept="*/*", input_serde=MessageSerde())(chat)

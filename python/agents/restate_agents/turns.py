"""
An agent's object (``agent``), as each integration makes it, and its turns.
A turn is the message its ``chat`` answers. While it answers, the object's
``status`` says so; ``stop`` cancels it; and ``steer`` adds messages to it,
which a framework gives its conversation at its next tool call
(``steering``). Those it did not take when it ends are answered as messages
of their own, unless it was cancelled.
"""

from __future__ import annotations

import asyncio
import json
import weakref
from collections.abc import Awaitable, Callable, Iterable
from contextvars import ContextVar
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

import restate_fns as restate

from restate_agents import log
from restate_agents.common import Messages, MessagesRequest, Synced, SyncRequest, Transcript
from restate_agents.object import TURN, Object, Stopped, Turn, context, refusal, service
from restate_agents.signals import Agent, CloseRequest, Steer, signals_of
from restate_agents.signals import close as close_turn
from restate_agents.signals import publish as publish_change
from restate_agents.signals import reset as reset_signals
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
    The conversation changed (a framework stored a message, say): its log
    takes the change, and the clients that follow it (``watch``, ``sync``)
    read it again. It is a call, so what the handler stored before is stored
    before they read it. Outside an agent's handler (a checkpointer's test,
    say), it does nothing.
    """
    try:
        ctx, name = context(), service()
    except RuntimeError:
        return
    if (transcript := _transcripts.get(name)) is not None:
        # One write at a time: a framework's tasks (tool calls in parallel, an SDK's own) publish at once.
        async with _writing.setdefault(ctx, asyncio.Lock()):
            await log.sync(ctx.db, (await transcript(ctx)).messages(), lambda: ctx.get(log.START))
    await ctx.object_call(publish_change, key=signals_of(name, ctx.key()), arg=None)


# The transcript of each agent, by its object's name: what its log is kept in step with.
_transcripts: dict[str, Callable[[restate.ObjectContext], Awaitable[Transcript]]] = {}

# The log's writes of each attempt, by its context.
_writing: weakref.WeakKeyDictionary[Any, asyncio.Lock] = weakref.WeakKeyDictionary()


def taken(ids: Iterable[str]) -> None:
    """The conversation has the steered messages ``ids``: the turn does not answer them as messages of their own."""
    if (turn := _answering.get()) is not None:
        turn.taken.update(ids)


def agent(
    name: str,
    answer: Callable[[restate.ObjectContext, str], Awaitable[str]],
    transcript: Callable[[restate.ObjectContext], Awaitable[Transcript]],
    **options: Any,
) -> Object:
    """
    An agent's object: ``chat``, which ``answer`` answers, a turn at a time;
    ``messages`` and ``sync``, its conversation as ``transcript`` gives it,
    from the log that each change takes (``publish``); and ``clear``, which
    forgets it. ``options``: those of ``Object``.
    """
    obj = Object(name, **options)
    _chat(obj, answer)
    _transcripts[name] = transcript

    async def own(ctx: Any, handler: str, arg: Any = None) -> Any:
        """A call of the agent's handler ``handler``, the same key."""
        answer = await ctx.generic_call(name, handler, arg=json.dumps(arg).encode(), key=ctx.key())
        return json.loads(answer) if answer else None

    async def messages(ctx: restate.ObjectContext, request: MessagesRequest | None = None) -> Messages:
        """The conversation so far, with the tool calls: a page of it, or what is new or changed since a cursor."""
        return await log.page(ctx.db, request, await ctx.get(log.START))

    async def sync(ctx: restate.ObjectContext, request: SyncRequest | None = None) -> Synced:
        """
        The conversation since ``cursor`` (by default, all of it), what the
        agent is doing, and the revision; with ``revision``, once there is a
        newer one, or at the timeout (``timeoutSeconds``: 50 by default, at
        most 60). A client follows the conversation by sending each answer's
        ``revision`` and ``cursor`` back, at once while it has ``more``.
        """
        request = request or {}
        if (revision := request.get("revision")) is not None:
            await own(ctx, "watch", {"revision": revision, "timeoutSeconds": request.get("timeoutSeconds", 50)})
        # The revision first: a change after it shows in the next sync. Then
        # the object's own handlers, which read its state as it is now.
        notifications = await own(ctx, "notifications")
        status = await own(ctx, "status")
        page = await own(ctx, "messages", {"cursor": request.get("cursor", 0), "limit": request.get("limit", 100)})
        return Synced(revision=notifications["revision"], status=status, **page)

    async def clear(ctx: restate.JournaledObjectSharedContext, _: None = None) -> Stopped:
        """
        Forget the conversation: stop the message being answered (as
        ``stop``), then clear the key's conversation, database and state, and
        what its signals hold (waits, kept signals, steered messages). The
        log's ids and cursors go on (see log.py): a follower reads on, and
        drops the messages before ``first``. A message sent before it is
        answered first, then forgotten too; one sent after starts anew.
        """
        turn = await ctx.get(TURN)
        if turn:
            ctx.cancel_invocation(turn["invocationId"])
        await own(ctx, "erase")
        return Stopped(stopped=turn["invocationId"] if turn else None)

    async def erase(ctx: restate.JournaledObjectContext, _: None = None) -> None:
        """``clear``'s work, once the handlers before it are done: the state (the database too), and the signals'."""
        page = await own(ctx, "messages", {"limit": 1})
        latest = page["messages"][-1]["id"] + 1 if page["messages"] else page.get("first", 0)
        ctx.clear_all()
        ctx.set(log.START, {"seq": page["cursor"], "id": latest})
        await ctx.object_call(reset_signals, key=signals_of(name, ctx.key()), arg=None)

    obj.handler("messages", kind="shared")(messages)
    obj.handler("sync", kind="shared")(sync)
    restate.VirtualObject.handler(obj, kind="shared", journal_replay=True)(clear)
    restate.VirtualObject.handler(obj, journal_replay=True, ingress_private=True)(erase)
    return obj


def _chat(obj: Object, answer: Callable[[restate.ObjectContext, str], Awaitable[str]]) -> None:
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
            if (terminal := refusal(error)) is None:
                raise
            end()
            raise terminal from error
        end()
        return answered

    obj.chat(chat)

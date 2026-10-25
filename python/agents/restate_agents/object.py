"""
The virtual object under an agent, ``Object``, for objects of your own too;
``ask``, which sends a message to an agent; and ``serve``.
"""

from __future__ import annotations

import asyncio
import inspect
import os
import threading
import weakref
from collections.abc import Awaitable, Callable
from contextvars import ContextVar
from datetime import timedelta
from typing import Any

import hypercorn.asyncio
import hypercorn.config
import restate_fns as restate
from pydantic import BaseModel, create_model
from restate.handler import RESTATE_UNIQUE_HANDLER_SYMBOL
from restate.serde import Serde
from restate_fns import InvocationRetryPolicy

from restate_agents.common import CHECK_MODEL, MessageSerde, ModelError, refused
from restate_agents.signals import (
    Agent,
    Notifications,
    Resolved,
    Signal,
    Steered,
    SteerRequest,
    Subscription,
    Watch,
    signals,
    signals_of,
    subscribe,
    unsubscribe,
)
from restate_agents.signals import notifications as notifications_of
from restate_agents.signals import pending as pending_signals
from restate_agents.signals import resolve as resolve_signal
from restate_agents.signals import steer as steer_signal

# Restate's UI hides the SQL storage from the journal view: the database's
# root and blobs in the object's state (v17/...), and the `sync` runs after its
# writes.
METADATA = {
    "dev.restate.tooling.journal.hidden.names": "sync",
    "dev.restate.tooling.journal.hidden.name.prefix": "v17/",
}

# Each attempt resumes from the database, so a failed one is retried for weeks: soon after a crash, then twice as
# long each time, up to 10 seconds apart (a provider's rate limit, say, is not asked again twice a second).
RETRY_POLICY = InvocationRetryPolicy(
    initial_interval=timedelta(milliseconds=50),
    exponentiation_factor=2,
    max_interval=timedelta(seconds=10),
    max_attempts=100_000,
)


# The name of the object whose handler runs.
_service: ContextVar[str] = ContextVar("restate_agents.service")

# The state key of the ``Turn`` being answered.
TURN = "restate-agents.turn"


class Turn(BaseModel):
    """The message a ``chat`` handler is answering: its invocation, and when it started."""

    invocationId: str  # noqa: N815 (as Restate names it)
    """The invocation's id: ``/restate/invocation/<id>/attach`` waits for its answer."""
    since: str
    """When it started, as an ISO date."""


class Status(BaseModel):
    """What an object is doing: the message it answers, if any, and what its tools wait for."""

    answering: Turn | None
    waiting: list[dict[str, Any]]


class Stopped(BaseModel):
    """What ``stop`` did: the invocation of the message it stopped, or None if none was answered."""

    stopped: str | None


class Object(restate.VirtualObject):
    """
    A Restate virtual object with a SQL database of its own, ``ctx.db``: one
    per key, stored with the object's state. ``options`` are those of
    ``restate_fns.VirtualObject``, which add to (or replace) the one set here:
    retries without end, since each attempt resumes from the database. Among
    them ``schema``, optional: tables of your own, beside the agent's (which
    are its own business), such as ``schema=["CREATE TABLE facts (fact TEXT)"]``:
    any name is yours but those starting with ``restate_``, the agent's
    (``restate_claude_messages``, ``restate_agent_log``...).

    Besides its own handlers, each has ``status``, what it is doing (the
    message it answers, and what its tools wait for), ``stop``, which stops
    that message, ``steer``, which adds to it, ``pending``, the waits of its
    tools (``signal`` in tools.py), and ``signal``, which resolves one; and
    ``notifications``, the revision of its conversation, which each change
    increases, and ``watch``, which waits for a newer one, for a client that
    follows it. They are shared, so they run while the object's handler
    works, and journaled: they use no ``ctx.db``.
    """

    def __init__(self, name: str, *, metadata: dict[str, str] | None = None, **options):
        super().__init__(
            name,
            metadata={**METADATA, **(metadata or {})},
            **{"invocation_retry_policy": RETRY_POLICY, **options},
        )

        async def signal(ctx: restate.JournaledObjectSharedContext, signal: Signal) -> Resolved:
            """Resolve a wait of this object's tools: by its id, or else the oldest for its name."""
            return await ctx.object_call(resolve_signal, key=signals_of(name, ctx.key()), arg=signal)

        async def pending(ctx: restate.JournaledObjectSharedContext, _: None = None) -> list[dict[str, Any]]:
            """The waits of this object's tools, oldest first, as ``Waiting``."""
            return await ctx.object_call(pending_signals, key=signals_of(name, ctx.key()), arg=None)

        async def status(ctx: restate.JournaledObjectSharedContext, _: None = None) -> Status:
            """What this object is doing: the message it answers, if any, and what its tools wait for."""
            turn = await ctx.get(TURN)
            return Status(
                answering=Turn(**turn) if turn else None,
                waiting=await ctx.object_call(pending_signals, key=signals_of(name, ctx.key()), arg=None),
            )

        async def stop(ctx: restate.JournaledObjectSharedContext, _: None = None) -> Stopped:
            """Cancel the invocation of the message being answered (``status``'s), as Restate's admin API does."""
            turn = await ctx.get(TURN)
            if turn:
                ctx.cancel_invocation(turn["invocationId"])
            return Stopped(stopped=turn["invocationId"] if turn else None)

        async def steer(ctx: restate.JournaledObjectSharedContext, text: str) -> Steered:
            """
            Add ``text`` to the message being answered (``status``'s), such as
            "Only Monday, please": the agent takes it at its next tool call. A
            turn that ends first answers it as a message of its own, as ``chat``
            does when no message is answered.
            """
            turn = await ctx.get(TURN)
            agent = Agent(service=name, key=ctx.key())
            request = SteerRequest(agent=agent, turn=turn["invocationId"] if turn else None, text=text)
            return await ctx.object_call(steer_signal, key=signals_of(name, ctx.key()), arg=request)

        async def notifications(ctx: restate.JournaledObjectSharedContext, _: None = None) -> Notifications:
            """The revision of the conversation: each change of what ``messages`` and ``status`` give increases it."""
            return await ctx.object_call(notifications_of, key=signals_of(name, ctx.key()), arg=None)

        async def watch(ctx: restate.JournaledObjectSharedContext, watch: Watch) -> Notifications:
            """
            The revision, once there is one newer than ``revision``, or at the timeout
            (``timeoutSeconds``, 50 by default, at most 60), whichever comes
            first: a client then reads what changed. It waits in Restate,
            holding no process; a client that sends it again with the same
            idempotency key joins the same wait.
            """
            key = signals_of(name, ctx.key())
            awakeable, changed = ctx.awakeable()
            ctx.object_send(subscribe, key=key, arg=Subscription(revision=watch.revision, awakeable=awakeable))
            timeout = ctx.sleep(timedelta(seconds=min(max(watch.timeoutSeconds, 0), 60)))
            match await restate.select(changed=changed, timeout=timeout):
                case ["changed", revision]:
                    return Notifications(**revision)
            ctx.object_send(unsubscribe, key=key, arg=awakeable)
            return await ctx.object_call(notifications_of, key=key, arg=None)

        for handler in (signal, pending, status, stop, notifications, watch):
            restate.VirtualObject.handler(self, kind="shared", journal_replay=True)(handler)
        restate.VirtualObject.handler(
            self, kind="shared", journal_replay=True, accept="*/*", input_serde=MessageSerde()
        )(steer)

    def handler(  # type: ignore[override]
        self, name: str | Callable | None = None, /, *, message: bool = False, **options: Any
    ) -> Any:
        """
        A handler, as ``@obj.handler``, ``@obj.handler("name")`` or
        ``@obj.handler(kind="shared")``. It takes ``ctx``, then its input: one
        parameter, or several, which a JSON object gives by name::

            @account.handler
            async def deposit(ctx, id: str, amount: int) -> int: ...

            curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'

        With ``message``, its input is a message, as ``chat``'s is: plain text
        or a JSON string (``curl -d 'Keep it short'``).
        """
        if callable(name):
            return self.handler(message=message, **options)(name)
        if message:
            options = {"accept": "*/*", "input_serde": MessageSerde(), **options}

        def register(fn: Callable) -> Callable:
            if (name or fn.__name__) in self.handlers:
                raise ValueError(f"{self.name} has a handler {name or fn.__name__} already: name yours otherwise")
            handler = self._handler(fn)
            if (model := getattr(handler, "input_model", None)) is not None:
                options.setdefault("input_serde", _ArgumentsSerde(model))
            registered = restate.VirtualObject.handler(self, name, **options)(handler)
            # The function itself names the handler, for calls and sends (ctx.object_call(add, ...)).
            vars(fn)[RESTATE_UNIQUE_HANDLER_SYMBOL] = vars(registered)[RESTATE_UNIQUE_HANDLER_SYMBOL]
            return fn

        return register

    def chat(self, fn: Callable) -> Callable:
        """
        ``fn`` as the ``chat`` handler, which takes its message as an agent's
        does, as plain text or as a JSON string (``curl -d 'Hi'``)::

            desk = Object("desk")

            @desk.chat
            async def chat(ctx, message: str) -> str: ...
        """
        return self.handler("chat", message=True)(fn)

    def _handler(self, fn: Callable) -> Callable:
        """``fn`` as the SDK takes a handler: one input at most."""
        signature = inspect.signature(fn, eval_str=True)
        ctx, *params = signature.parameters.values()

        if len(params) == 1:

            async def handler(context: Any, *input: Any) -> Any:
                _service.set(self.name)
                return await _answered(fn(context, *input))

            handler.__signature__ = signature  # type: ignore[attr-defined]
        elif not params:
            # No input, but a body is taken (and left unread), as TypeScript's handlers take one: any client calls it.
            async def handler(context: Any, _: None = None) -> Any:  # type: ignore[misc]
                _service.set(self.name)
                return await _answered(fn(context))

            handler.__signature__ = signature.replace(  # type: ignore[attr-defined]
                parameters=[
                    ctx,
                    inspect.Parameter("_", inspect.Parameter.POSITIONAL_OR_KEYWORD, default=None, annotation=Any),
                ]
            )
        else:
            fields: dict[str, Any] = {
                p.name: (
                    p.annotation if p.annotation is not p.empty else Any,
                    ... if p.default is p.empty else p.default,
                )
                for p in params
            }
            model = create_model(f"{fn.__name__}_input", **fields)

            async def handler(context: Any, input: Any) -> Any:  # type: ignore[misc]
                _service.set(self.name)
                return await _answered(fn(context, **{name: getattr(input, name) for name in fields}))

            handler.__signature__ = signature.replace(  # type: ignore[attr-defined]
                parameters=[ctx, inspect.Parameter("input", inspect.Parameter.POSITIONAL_OR_KEYWORD, annotation=model)]
            )
            handler.input_model = model  # type: ignore[attr-defined]
        handler.__name__ = fn.__name__
        handler.__doc__ = fn.__doc__
        return handler


class _ArgumentsSerde(Serde[Any]):
    """A handler's parameters, as a JSON object: from a dict, such as a call's, or the model of the parameters."""

    def __init__(self, model: type[BaseModel]) -> None:
        self.model = model

    def deserialize(self, buf: bytes) -> Any:
        return self.model.model_validate_json(buf or b"{}")

    def serialize(self, obj: Any) -> bytes:
        return self.model.model_validate(obj).model_dump_json().encode()


# The messages each attempt sent, by its context.
_asked: weakref.WeakKeyDictionary[Any, int] = weakref.WeakKeyDictionary()


async def ask(
    ctx: Any, agent: restate.VirtualObject, message: str, *, key: str | None = None, idempotency_key: str | None = None
) -> str:
    """
    Send ``message`` to ``agent`` (its ``chat``), keyed ``key`` (by default,
    this object's key), and return its answer::

        answer = await ask(ctx, billing, "I was charged twice")

    It is sent once per invocation, however often an attempt runs: a handler
    is not replayed, so the call has an idempotency key, which a retried
    attempt's call joins (by default the invocation's id, the agent's name
    and how many messages the attempt sent before).
    """
    count = _asked[ctx] = _asked.get(ctx, 0) + 1
    h = agent.handlers["chat"]
    result = await ctx.generic_call(
        agent.name,
        h.name,
        arg=h.handler_io.input_serde.serialize(message),
        key=key or ctx.key(),
        idempotency_key=idempotency_key or f"{ctx.request().id}:{agent.name}:{count}",
    )
    return h.handler_io.output_serde.deserialize(result)


def context() -> Any:
    """The context of the handler running, such as a tool's: ``ctx.db``, calls and sends."""
    from restate.extensions import current_context

    ctx = current_context()
    if ctx is None:
        raise RuntimeError("not in a Restate handler")
    return ctx


def service() -> str:
    """The name of the object whose handler runs, such as a tool's."""
    try:
        return _service.get()
    except LookupError:
        raise RuntimeError("not in a handler of an agent's object") from None


def serve(*services: restate.Service | restate.VirtualObject | restate.Workflow, background: bool = False) -> None:
    """
    Serve ``services``, and the agents' ``signals``, over HTTP/2, on PORT or
    9080, until interrupted; or, with ``background``, on a thread of its own,
    for as long as the process lives, returning at once (in a Modal Server's
    ``@modal.enter()``, say).
    """
    config = hypercorn.config.Config()
    config.bind = [f"0.0.0.0:{os.environ.get('PORT', '9080')}"]
    if not background:
        asyncio.run(hypercorn.asyncio.serve(app(*services), config))  # type: ignore[arg-type]
        return

    async def forever() -> None:
        # Off the main thread, hypercorn cannot wait for a signal to stop: the process's end stops it.
        await asyncio.Event().wait()

    server = hypercorn.asyncio.serve(app(*services), config, shutdown_trigger=forever)  # type: ignore[arg-type]
    threading.Thread(target=asyncio.run, args=(server,), name="restate", daemon=True).start()


def app(*services: restate.Service | restate.VirtualObject | restate.Workflow, **options: Any) -> Any:
    """The ASGI app of ``services``, and of the agents' ``signals``; ``options`` are ``restate.app``'s."""
    return restate.app([*services, *([] if signals in services else [signals])], **options)


def refusal(error: BaseException) -> restate.TerminalError | None:
    """
    ``error`` as a terminal error (424, Failed Dependency), if a retry does
    not change it: a model the agent cannot take, or its provider's refusal
    (``refused``).
    """
    if isinstance(error, restate.TerminalError) or not refused(error):
        return None
    if isinstance(error, ModelError):
        return restate.TerminalError(str(error), status_code=424)
    return restate.TerminalError(f"the model's provider refused the request: {CHECK_MODEL}. {error}", status_code=424)


async def _answered(answer: Awaitable[Any]) -> Any:
    """A handler's answer: a model provider's refusal (a missing API key, an unknown model) ends its invocation."""
    try:
        return await answer
    except Exception as error:
        if (terminal := refusal(error)) is not None:
            raise terminal from error
        raise

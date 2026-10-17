"""
The virtual object under an agent, ``Object``, and ``serve``. ``Object`` is
internal for now: the agents are built on it, and the tests use it.
"""

from __future__ import annotations

import asyncio
import inspect
import os
from collections.abc import Callable
from contextvars import ContextVar
from datetime import timedelta
from typing import Any

import hypercorn.asyncio
import hypercorn.config
import restate
from pydantic import BaseModel, create_model
from restate import InvocationRetryPolicy
from restate.handler import RESTATE_UNIQUE_HANDLER_SYMBOL
from restate.serde import Serde

from restate_agents.signals import Resolved, Signal, signals, signals_of
from restate_agents.signals import pending as pending_signals
from restate_agents.signals import resolve as resolve_signal

# Restate's UI hides the SQL storage from the journal view: the database's
# pages, log and header in the object's state (v13/...), the sends pending
# with a transaction, and the `sync` run before an attempt ends.
METADATA = {
    "dev.restate.tooling.journal.hidden.names": "sync,_restate_sql_sends",
    "dev.restate.tooling.journal.hidden.name.prefix": "v13/",
}

# Each attempt resumes from the database, so a failed one is retried soon, and without end.
RETRY_POLICY = InvocationRetryPolicy(
    initial_interval=timedelta(milliseconds=50), max_interval=timedelta(milliseconds=500), max_attempts=100_000
)


# The name of the object whose handler runs.
_service: ContextVar[str] = ContextVar("restate_agents.service")


class Object(restate.VirtualObject):
    """
    A Restate virtual object with a SQL database of its own, ``ctx.db``: one
    per key, stored with the object's state. ``options`` are those of
    ``restate.VirtualObject``, which add to (or replace) the ones set here:
    ``restate.sql_options``, and retries without end, since each attempt
    resumes from the database.

    Each has the handlers ``signal``, which resolves a wait of its tools (see
    ``signal`` in tools.py), and ``pending``, the waits. Both are shared, so
    they run while the object's handler waits, and journaled: they use no
    ``ctx.db``.
    """

    def __init__(self, name: str, *, metadata: dict[str, str] | None = None, **options):
        super().__init__(
            name,
            metadata={**METADATA, **(metadata or {})},
            **{**restate.sql_options, "invocation_retry_policy": RETRY_POLICY, **options},
        )

        async def signal(ctx: restate.ObjectSharedContext, signal: Signal) -> Resolved:
            """Resolve a wait of this object's tools: by its id, or else the oldest for its name."""
            return await ctx.object_call(resolve_signal, key=signals_of(name, ctx.key()), arg=signal)

        async def pending(ctx: restate.ObjectSharedContext) -> list[dict[str, Any]]:
            """The waits of this object's tools, oldest first, as ``Waiting``."""
            return await ctx.object_call(pending_signals, key=signals_of(name, ctx.key()), arg=None)

        for handler in (signal, pending):
            restate.VirtualObject.handler(self, kind="shared", journal_replay=True)(handler)

    def handler(self, name: str | Callable | None = None, /, **options: Any) -> Any:  # type: ignore[override]
        """
        A handler, as ``@obj.handler``, ``@obj.handler("name")`` or
        ``@obj.handler(kind="shared")``. It takes ``ctx``, then its input: one
        parameter, or several, which a JSON object gives by name::

            @account.handler
            async def deposit(ctx, id: str, amount: int) -> int: ...

            curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
        """
        if callable(name):
            return self.handler()(name)

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

    def _handler(self, fn: Callable) -> Callable:
        """``fn`` as the SDK takes a handler: one input at most."""
        signature = inspect.signature(fn, eval_str=True)
        ctx, *params = signature.parameters.values()

        if len(params) <= 1:

            async def handler(context: Any, *input: Any) -> Any:
                _service.set(self.name)
                return await fn(context, *input)

            handler.__signature__ = signature  # type: ignore[attr-defined]
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
                return await fn(context, **{name: getattr(input, name) for name in fields})

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


def serve(*services: restate.Service | restate.VirtualObject | restate.Workflow) -> None:
    """Serve ``services``, and the agents' ``signals``, over HTTP/2, on PORT or 9080, until interrupted."""
    config = hypercorn.config.Config()
    config.bind = [f"0.0.0.0:{os.environ.get('PORT', '9080')}"]
    asyncio.run(hypercorn.asyncio.serve(app(*services), config))  # type: ignore[arg-type]


def app(*services: restate.Service | restate.VirtualObject | restate.Workflow, **options: Any) -> Any:
    """The ASGI app of ``services``, and of the agents' ``signals``; ``options`` are ``restate.app``'s."""
    return restate.app([*services, *([] if signals in services else [signals])], **options)

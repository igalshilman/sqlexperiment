"""
Restate as steps of a graph of your own: nodes that call a handler, send to
one, run a workflow, or wait for a signal; where the graph, rather than a
model, decides when::

    from restate_agents.langgraph import nodes

    graph.add_node("approval", nodes.signal("approval", input=lambda state: state["draft"], update="verdict"))
    graph.add_node("publish", nodes.workflow(publishing, input=lambda state: state["draft"], update="url"))
    graph.add_edge("write", "approval")

Each takes ``input``, a function of the graph's state giving the handler's
input (none, if not given); and ``update``, where its result goes: a key of
the state, or a function of the state and the result giving the update
(nothing, if not given). The result is the handler's, as its output type
says (a pydantic model, say), not text.

Each is made once per run of its node, however often an attempt crashes:
its idempotency key is ``<invocation id>:<task id>``, and LangGraph gives a
step that a crash cut off the same task id when it runs it again. A
handler's ``restate.TerminalError`` fails the turn (no model reads it): catch
it in a node of your own to carry on. A cancelled turn (``stop``) ends at a
node that waits, as at a tool that does.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from datetime import timedelta
from typing import Any

import restate_fns as restate
from langgraph.runtime import get_runtime
from restate.handler import Handler

from restate_agents.object import context, service
from restate_agents.signals import Waiting, signals_of
from restate_agents.signals import wait as wait_for_signal
from restate_agents.tools import handler_of, main_of

State = Any
Node = Callable[[State], Awaitable[dict[str, Any]]]
Input = Callable[[State], Any] | None
Key = str | Callable[[State], str] | None
Update = str | Callable[[State, Any], dict[str, Any]] | None


def call(handler: Callable[..., Any], *, input: Input = None, key: Key = None, update: Update = None) -> Node:
    """
    A node that calls ``handler`` (of a service, an object, or a workflow),
    and puts its result in the state::

        graph.add_node("status", nodes.call(status, input=lambda state: state["order"], update="status"))

    ``key``: the object's or the workflow's key, or a function of the state giving it.
    """
    h = handler_of(handler)
    _keyed(h, key)

    async def run(state: State) -> dict[str, Any]:
        ctx = context()
        result = await ctx.generic_call(
            h.service_tag.name,
            h.name,
            arg=_request(h, input, state),
            key=_key(key, state),
            idempotency_key=idempotency_key(ctx),
        )
        return _update(update, state, h.handler_io.output_serde.deserialize(result))

    return run


def send(handler: Callable[..., Any], *, input: Input = None, key: Key = None, delay: timedelta | None = None) -> Node:
    """
    A node that sends a request to ``handler`` and does not wait for it, as
    ``call`` calls it; after ``delay``, if given::

        graph.add_node("follow_up", nodes.send(checkin, input=lambda state: state["order"], delay=timedelta(days=1)))
    """
    h = handler_of(handler)
    _keyed(h, key)

    async def run(state: State) -> dict[str, Any]:
        ctx = context()
        ctx.generic_send(
            h.service_tag.name,
            h.name,
            arg=_request(h, input, state),
            key=_key(key, state),
            send_delay=delay,
            idempotency_key=idempotency_key(ctx),
        )
        return {}

    return run


def workflow(target: restate.Workflow, *, input: Input = None, key: Key = None, update: Update = None) -> Node:
    """
    A node that runs the workflow ``target`` (its ``main`` handler) and puts
    its result in the state::

        graph.add_node("publish", nodes.workflow(publishing, input=lambda state: state["draft"], update="url"))

    Each run of the node runs a workflow of its own, keyed ``<this key>-<task
    id>``, unless ``key`` gives the key.
    """
    h = main_of(target)

    async def run(state: State) -> dict[str, Any]:
        ctx = context()
        result = await ctx.generic_call(
            target.name,
            h.name,
            arg=_request(h, input, state),
            key=_key(key, state) or f"{ctx.key()}-{get_runtime().execution_info.task_id}",
            idempotency_key=idempotency_key(ctx),
        )
        return _update(update, state, h.handler_io.output_serde.deserialize(result))

    return run


def signal(signal: str, *, input: Input = None, update: Update = None) -> Node:
    """
    A node that waits for the signal ``signal`` to the agent, and puts its
    value in the state: a person's approval, say. ``input``: what it waits
    for, which ``pending`` shows::

        graph.add_node("approval", nodes.signal("approval", input=lambda state: state["draft"], update="verdict"))

        curl localhost:8080/writer/post/pending
        curl localhost:8080/writer/post/signal --json '{"name": "approval", "value": {"approved": true}}'

    The agent waits without holding a process, as the ``signal`` tool does.
    """

    async def run(state: State) -> dict[str, Any]:
        ctx = context()
        id = idempotency_key(ctx)
        waiting = Waiting(id=id, name=signal, input=input(state) if input else None)
        value = await ctx.object_call(
            wait_for_signal, key=signals_of(service(), ctx.key()), arg=waiting, idempotency_key=id
        )
        return _update(update, state, value)

    return run


def idempotency_key(ctx: Any) -> str:
    """The idempotency key of the node running, in this invocation: the same when a crash runs it again."""
    return f"{ctx.request().id}:{get_runtime().execution_info.task_id}"


def _keyed(h: Handler, key: Key) -> None:
    if key is None and h.service_tag.kind in ("object", "workflow"):
        raise TypeError(f"{h.service_tag.name}/{h.name} is an {h.service_tag.kind}'s handler: give the node its key")


def _key(key: Key, state: State) -> str | None:
    return key(state) if callable(key) else key


def _request(h: Handler, input: Input, state: State) -> bytes:
    return h.handler_io.input_serde.serialize(input(state) if input else None)


def _update(update: Update, state: State, result: Any) -> dict[str, Any]:
    if update is None:
        return {}
    return {update: result} if isinstance(update, str) else update(state, result)

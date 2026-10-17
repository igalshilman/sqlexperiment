"""
Tools for an agent of any framework here: ``tool``, a function of your own;
and Restate's, which call, send to, start and wait for what Restate runs:
``call``, ``send``, ``workflow``, ``subagent`` and ``signal``.
"""

from __future__ import annotations

import inspect
from collections.abc import Callable
from dataclasses import dataclass
from datetime import timedelta
from typing import Annotated, Any

import restate
from pydantic import BaseModel, Field, create_model
from pydantic_core import to_json
from restate.handler import Handler, handler_from_callable

from restate_agents.object import context, service
from restate_agents.signals import Waiting, signals_of
from restate_agents.signals import wait as wait_for_signal

# Parameters a tool gets from where it runs, rather than from the model.
INJECTED = ("ctx", "call_id", "idempotency_key")

# A key: a string, or a function of the handler's context giving one.
Key = str | Callable[[Any], str]


def idempotency_key(ctx: Any, call_id: str) -> str:
    """The idempotency key of tool call ``call_id`` in this invocation: the same in each attempt."""
    return f"{ctx.request().id}:{call_id}"


@dataclass(frozen=True)
class Tool:
    """A tool: its name, its description (the function's docstring), its input, and the function."""

    name: str
    description: str
    input: type[BaseModel]
    fn: Callable[..., Any]

    async def run(self, input: dict[str, Any], call_id: str) -> Any:
        """Call the function with ``input``, and ``ctx``, ``call_id`` and ``idempotency_key`` if it takes them."""
        params = inspect.signature(self.fn).parameters
        injected = {
            "ctx": context,
            "call_id": lambda: call_id,
            "idempotency_key": lambda: idempotency_key(context(), call_id),
        }
        result = self.fn(**input, **{name: get() for name, get in injected.items() if name in params})
        return await result if inspect.isawaitable(result) else result


def tool(fn: Callable[..., Any] | None = None, *, name: str | None = None, description: str | None = None) -> Any:
    """
    A tool, from a function: the model gives its parameters, described by
    their types and the docstring::

        @tool
        async def remember(ctx, call_id, fact: str) -> str:
            '''Remember a fact about the user.'''
            await ctx.db.execute("INSERT OR IGNORE INTO facts VALUES (?, ?)", [call_id, fact])
            return f"Remembered: {fact}"

    Three parameters are not the model's: ``ctx``, the handler's context
    (``ctx.db``, calls, sends); ``call_id``, the tool call's id; and
    ``idempotency_key``, the call's in this invocation. A crash runs an
    unfinished tool call again with the same id: a tool that must not run
    twice passes it on, as an idempotency key or a row's key. A
    ``restate.TerminalError`` is the tool's answer to the model; any other
    error fails the attempt, which Restate retries.
    """
    if fn is None:
        return lambda fn: tool(fn, name=name, description=description)
    params = inspect.signature(fn, eval_str=True).parameters.values()
    fields: dict[str, Any] = {
        p.name: (p.annotation if p.annotation is not p.empty else Any, ... if p.default is p.empty else p.default)
        for p in params
        if p.name not in INJECTED
    }
    return Tool(
        name=name or fn.__name__,
        description=description or inspect.getdoc(fn) or fn.__name__,
        input=create_model(fn.__name__, **fields),
        fn=fn,
    )


def call(
    handler: Callable[..., Any], *, description: str | None = None, name: str | None = None, key: Key | None = None
) -> Tool:
    """
    A tool that calls ``handler``, a handler of a service, an object or a
    workflow, and answers with what it returns::

        @orders.handler()
        async def status(ctx, order: str) -> str:
            '''The status of an order.'''

        support = langgraph.agent("support", tools=[call(status)])

    The model gives the handler's input: its parameter, or the fields of its
    pydantic model; and the key, of an object or a workflow, unless ``key``
    gives it. ``description`` is the handler's docstring unless given;
    ``name``, ``<service>_<handler>``. The call is made once per tool call,
    however often an attempt crashes (its idempotency key is the tool
    call's); a ``restate.TerminalError`` of the handler is the tool's answer.
    """
    h = _handler(handler)
    target = _Target(h, key)

    async def run(ctx: Any, call_id: str, **input: Any) -> str:
        k, arg = target.split(ctx, input)
        result = await ctx.generic_call(
            h.service_tag.name, h.name, arg=arg, key=k, idempotency_key=idempotency_key(ctx, call_id)
        )
        return _text(h.handler_io.output_serde.deserialize(result))

    return target.tool(name or f"{h.service_tag.name}_{h.name}", description, run)


def send(
    handler: Callable[..., Any],
    *,
    description: str | None = None,
    name: str | None = None,
    key: Key | None = None,
    delay: timedelta | None = None,
) -> Tool:
    """
    A tool that sends a request to ``handler`` and does not wait for it, as
    ``call`` calls it; after ``delay``, if given::

        send(checkin, name="schedule_checkin", delay=timedelta(days=1))
    """
    h = _handler(handler)
    target = _Target(h, key)

    async def run(ctx: Any, call_id: str, **input: Any) -> str:
        k, arg = target.split(ctx, input)
        ctx.generic_send(
            h.service_tag.name, h.name, arg=arg, key=k, send_delay=delay, idempotency_key=idempotency_key(ctx, call_id)
        )
        return f"sent to {h.service_tag.name}{f'/{k}' if k else ''}/{h.name}"

    return target.tool(name or f"{h.service_tag.name}_{h.name}", description, run)


def workflow(
    target: restate.Workflow, *, description: str | None = None, name: str | None = None, key: Key | None = None
) -> Tool:
    """
    A tool that runs the workflow ``target`` and answers with its result::

        workflow(refund, description="Refund an order, once a manager approved it.")

    The model gives the input of its ``main`` handler. Each tool call runs a
    workflow of its own, keyed ``<this key>-<call id>``, unless ``key`` gives
    the key.
    """
    h = next(h for h in target.handlers.values() if h.kind == "workflow")
    own = _Target(h, key, model_gives_key=False)

    async def run(ctx: Any, call_id: str, **input: Any) -> str:
        k, arg = own.split(ctx, input)
        result = await ctx.generic_call(
            target.name,
            h.name,
            arg=arg,
            key=k or f"{ctx.key()}-{call_id}",
            idempotency_key=idempotency_key(ctx, call_id),
        )
        return _text(h.handler_io.output_serde.deserialize(result))

    return own.tool(name or target.name, description, run)


def subagent(
    agent: restate.VirtualObject, *, description: str | None = None, name: str | None = None, handler: str = "chat"
) -> Tool:
    """
    A tool that hands a task to ``agent`` (an agent, or any object with a
    ``chat`` handler, or ``handler``), and answers with its answer::

        editor = langgraph.agent("editor", tools=[subagent(researcher, description="a researcher")])

    Each task has a name: the object keyed ``<this key>.<task name>`` does
    it. Tasks of different names run at once; the same name continues the
    same task.
    """
    h = agent.handlers[handler]

    async def run(ctx: Any, call_id: str, name: str, task: str) -> str:
        result = await ctx.generic_call(
            agent.name,
            h.name,
            arg=h.handler_io.input_serde.serialize(task),
            key=f"{ctx.key()}.{name}",
            idempotency_key=idempotency_key(ctx, call_id),
        )
        return _text(h.handler_io.output_serde.deserialize(result))

    return Tool(
        name=name or agent.name,
        description=f"Hand a task to {description or f'the {agent.name} agent'}, and get its answer. "
        "Tasks of different names run at once; the same name continues the same task.",
        input=create_model(
            f"{agent.name}_task",
            name=(
                Annotated[
                    str, Field(pattern=r"^[a-z0-9-]+$", description='A short name for the task, such as "fizzbuzz"')
                ],
                ...,
            ),
            task=(Annotated[str, Field(description="The task in full: the agent sees nothing else")], ...),
        ),
        fn=run,
    )


def signal(signal: str, *, description: str, input: type[BaseModel] | None = None, name: str | None = None) -> Tool:
    """
    A tool that waits for the signal ``signal`` to the agent, and answers with
    its value: a person's approval, say, or another system's result. The
    agent waits without holding a process. With an ``input``, the model says
    what it waits for (such as the refund to approve), which ``pending``
    shows::

        class Refund(BaseModel):
            order: str
            amount: float

        signal("approval", description="Wait for a manager to approve a refund.", input=Refund)

    Each wait has an id: the agent's ``pending`` handler lists them, and its
    ``signal`` handler resolves one, by its id, or the oldest wait for a name::

        curl localhost:8080/support/alice/pending
        curl localhost:8080/support/alice/signal --json '{"id": "inv_1f…:toolu_01", "value": "approved"}'
        curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'

    It answers with the id of the wait it resolved, or null if none waited:
    the signal is dropped then, unless it has ``"keep": true``, which keeps
    it for the next wait for its name.
    """

    async def run(ctx: Any, call_id: str, **given: Any) -> str:
        id = idempotency_key(ctx, call_id)
        waiting = Waiting(id=id, name=signal, input=given if input is not None else None)
        value = await ctx.object_call(
            wait_for_signal, key=signals_of(service(), ctx.key()), arg=waiting, idempotency_key=id
        )
        return _text(value)

    return Tool(
        name=name or f"wait_for_{signal}",
        description=description,
        input=create_model(f"wait_for_{signal}", __base__=input) if input else create_model(f"wait_for_{signal}"),
        fn=run,
    )


def _handler(handler: Callable[..., Any]) -> Handler:
    try:
        return handler_from_callable(handler)
    except ValueError:
        raise TypeError(
            f"{handler!r} is not a Restate handler: pass a function that a handler decorator made one"
        ) from None


class _Target:
    """A handler as a tool's target: the model's input for it, and the key and request it gives."""

    def __init__(self, h: Handler, key: Key | None, model_gives_key: bool = True) -> None:
        self.h = h
        self.key = key
        self.keyed = model_gives_key and key is None and h.service_tag.kind in ("object", "workflow")
        self.fields: dict[str, Any] = {}
        self.make: Callable[[dict[str, Any]], Any] = lambda input: None
        if h.arity == 1:
            return
        annotation = h.handler_io.input_type.annotation if h.handler_io.input_type else Any
        if isinstance(annotation, type) and issubclass(annotation, BaseModel):
            self.fields = {n: (f.annotation, f) for n, f in annotation.model_fields.items()}
            self.make = annotation.model_validate
        else:
            param = list(inspect.signature(h.fn).parameters.values())[1]
            self.fields = {param.name: (annotation, ... if param.default is param.empty else param.default)}
            self.make = lambda input: input[param.name]

    def tool(self, name: str, description: str | None, run: Callable[..., Any]) -> Tool:
        description = description or self.h.description or inspect.getdoc(self.h.fn)
        if not description:
            raise TypeError(f"{self.h.service_tag.name}/{self.h.name} has no docstring: give the tool a description")
        fields = dict(self.fields)
        if self.keyed:
            fields = {
                "key": (Annotated[str, Field(description=f"Which {self.h.service_tag.name}: its key")], ...),
                **fields,
            }
        return Tool(name=name, description=description, input=create_model(name, **fields), fn=run)

    def split(self, ctx: Any, input: dict[str, Any]) -> tuple[str | None, bytes]:
        """The key (if the target has one) and the request, from the model's input."""
        input = dict(input)
        key = input.pop("key") if self.keyed else self.key(ctx) if callable(self.key) else self.key
        return key, self.h.handler_io.input_serde.serialize(self.make(input))


def _text(value: Any) -> str:
    """A result, as the model reads it: a string as it is, anything else as JSON, and None as "done"."""
    if value is None:
        return "done"
    return value if isinstance(value, str) else to_json(value).decode()

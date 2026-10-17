"""``tool``: a function an agent of any framework here can call."""

from __future__ import annotations

import inspect
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from pydantic import BaseModel, create_model

from restate_agents.object import context

# Parameters a tool gets from where it runs, rather than from the model.
INJECTED = ("ctx", "call_id")


@dataclass(frozen=True)
class Tool:
    """A tool: its name, its description (the function's docstring), its input, and the function."""

    name: str
    description: str
    input: type[BaseModel]
    fn: Callable[..., Any]

    async def run(self, input: dict[str, Any], call_id: str) -> Any:
        """Call the function with ``input``, and ``ctx`` and ``call_id`` if it takes them."""
        params = inspect.signature(self.fn).parameters
        injected = {"ctx": context, "call_id": lambda: call_id}
        result = self.fn(**input, **{name: get() for name, get in injected.items() if name in params})
        return await result if inspect.isawaitable(result) else result


def tool(fn: Callable[..., Any] | None = None, *, name: str | None = None, description: str | None = None) -> Any:
    """
    A tool, from a function: the model gives its parameters, described by
    their types and the docstring::

        @tool
        async def send_money(ctx, call_id, to: str, amount: int) -> str:
            '''Send money from the customer's account to another account.'''
            await ctx.object_call(transfer, key=ctx.key(), arg={"id": call_id, "to": to, "amount": amount})
            return f"Sent {amount} to {to}"

    Two parameters are not the model's: ``ctx``, the handler's context
    (``ctx.db``, calls, sends), and ``call_id``, the tool call's id. A crash
    runs an unfinished tool call again with the same id: a tool that must not
    run twice passes it on, as an idempotency key or a row's key. A
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

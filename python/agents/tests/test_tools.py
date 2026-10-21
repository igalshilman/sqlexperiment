"""Restate's tools for agents, on a fake context: their names, the model's input, and the requests they make."""

import json
from datetime import timedelta
from typing import Any

import pytest
import restate_sql as restate
from pydantic import BaseModel
from restate_agents import call, send, signal, subagent, tool, workflow
from restate_agents.object import _service

orders = restate.Service("orders")
carts = restate.VirtualObject("carts")
refunds = restate.Workflow("refund")


@orders.handler()
async def status(ctx: restate.Context, order: str) -> str:
    """The status of an order."""
    return ""


class Item(BaseModel):
    sku: str
    count: int = 1


@carts.handler()
async def add(ctx: restate.ObjectContext, item: Item) -> list[str]:
    """Add an item to a cart."""
    return []


@carts.handler()
async def clear(ctx: restate.ObjectContext) -> None:
    pass


class Refund(BaseModel):
    order: str
    amount: float


@refunds.main()
async def run(ctx: restate.WorkflowContext, refund: Refund) -> str:
    return ""


assistant = restate.VirtualObject("assistant")


@assistant.handler()
async def chat(ctx: restate.ObjectContext, message: str) -> str:
    return ""


class Request:
    id = "inv1"


class Context:
    """What the tools use of an agent's context, and the calls and sends they make."""

    def __init__(self, answer: Any = None) -> None:
        self.answer = answer
        self.calls: list[dict[str, Any]] = []

    def key(self) -> str:
        return "alice"

    def request(self) -> Request:
        return Request()

    async def generic_call(self, service: str, handler: str, arg: bytes, **options: Any) -> bytes:
        self.calls.append({"to": f"{service}/{handler}", "arg": json.loads(arg) if arg else None, **options})
        return json.dumps(self.answer).encode()

    def generic_send(self, service: str, handler: str, arg: bytes, **options: Any) -> None:
        self.calls.append({"to": f"{service}/{handler}", "arg": json.loads(arg) if arg else None, **options})

    async def object_call(self, handler: Any, key: str, arg: Any, idempotency_key: str) -> Any:
        self.calls.append({"key": key, "arg": arg.model_dump(), "idempotency_key": idempotency_key})
        return self.answer


def fields(t) -> list[str]:
    return list(t.input.model_json_schema().get("properties", {}))


async def run_tool(t, ctx: Context, input: dict[str, Any], call_id: str = "call_1") -> Any:
    return await t.fn(ctx=ctx, call_id=call_id, **input)


async def test_call_a_service():
    t = call(status)
    assert (t.name, t.description, fields(t)) == ("orders_status", "The status of an order.", ["order"])
    ctx = Context("delivered")
    assert await run_tool(t, ctx, {"order": "103"}) == "delivered"
    assert ctx.calls == [{"to": "orders/status", "arg": "103", "key": None, "idempotency_key": "inv1:call_1"}]


async def test_call_an_object_the_model_keys():
    t = call(add)
    assert fields(t) == ["key", "sku", "count"]
    ctx = Context(["tea"])
    assert await run_tool(t, ctx, {"key": "bob", "sku": "tea"}) == '["tea"]'
    assert ctx.calls[0] | {"idempotency_key": None} == {
        "to": "carts/add",
        "arg": {"sku": "tea", "count": 1},
        "key": "bob",
        "idempotency_key": None,
    }


async def test_call_an_object_keyed_by_a_function():
    t = call(clear, description="Empty the user's cart.", key=lambda ctx: ctx.key())
    assert fields(t) == []
    ctx = Context()
    assert await run_tool(t, ctx, {}) == "done"
    assert (ctx.calls[0]["key"], ctx.calls[0]["arg"]) == ("alice", None)


def test_a_handler_without_a_docstring_needs_a_description():
    with pytest.raises(TypeError, match="give the tool a description"):
        call(clear)


async def test_send_with_a_delay():
    t = send(status, name="check_later", delay=timedelta(hours=1))
    ctx = Context()
    assert await run_tool(t, ctx, {"order": "103"}) == "sent to orders/status"
    assert (t.name, ctx.calls[0]["send_delay"]) == ("check_later", timedelta(hours=1))


async def test_a_workflow_per_tool_call():
    t = workflow(refunds, description="Refund an order.")
    assert (t.name, fields(t)) == ("refund", ["order", "amount"])
    ctx = Context("refunded")
    assert await run_tool(t, ctx, {"order": "103", "amount": 5}) == "refunded"
    assert (ctx.calls[0]["to"], ctx.calls[0]["key"]) == ("refund/run", "alice-call_1")


async def test_a_subagents_task_is_keyed_by_its_name():
    t = subagent(assistant, description="an assistant")
    assert (t.name, fields(t)) == ("assistant", ["name", "task"])
    ctx = Context("hi")
    assert await run_tool(t, ctx, {"name": "greet", "task": "say hi"}) == "hi"
    assert (ctx.calls[0]["to"], ctx.calls[0]["key"], ctx.calls[0]["arg"]) == ("assistant/chat", "alice.greet", "say hi")


async def test_a_signal_waits_in_the_agents_signals():
    t = signal("approval", description="Wait for an approval.", input=Refund)
    assert (t.name, fields(t)) == ("wait_for_approval", ["order", "amount"])
    ctx = Context("approved")
    token = _service.set("support")
    try:
        assert await run_tool(t, ctx, {"order": "103", "amount": 5}) == "approved"
    finally:
        _service.reset(token)
    [wait] = ctx.calls
    assert wait["key"] == "support:alice" and wait["idempotency_key"] == "inv1:call_1"
    assert (wait["arg"]["id"], wait["arg"]["name"], wait["arg"]["input"]) == (
        "inv1:call_1",
        "approval",
        {"order": "103", "amount": 5},
    )


def test_a_tools_injected_parameters_are_not_the_models():
    @tool
    async def remember(ctx, call_id, idempotency_key, fact: str) -> str:
        """Remember a fact."""
        return fact

    assert fields(remember) == ["fact"]

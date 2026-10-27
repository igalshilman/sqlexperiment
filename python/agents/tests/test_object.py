"""Objects of your own: a ``chat`` handler of yours, and ``ask``, which sends a message to an agent once."""

import pytest
from fakes import ClaudeClient, Object, Teller
from restate_agents import Object as AgentObject
from restate_agents import ask, claude


def agents():
    """A desk that sends each message to billing, twice."""
    billing = claude.agent("billing", client=ClaudeClient(Teller()))
    desk = AgentObject("desk")

    @desk.chat
    async def chat(ctx, message: str) -> list[str]:
        return [await ask(ctx, billing, message), await ask(ctx, billing, message, key="bob")]

    return billing, desk


async def test_ask_sends_to_the_agents_chat_with_a_key_that_names_the_call():
    _, desk = agents()
    alice = Object()
    alice.answer = lambda handler, request: b'"Noted."'
    assert await alice.invoke(desk.handlers["chat"].fn, "I was charged twice", invocation="inv-1") == [
        "Noted.",
        "Noted.",
    ]
    assert [(s, h, k, i, a) for s, h, k, i, a in alice.calls] == [
        ("billing", "chat", "alice", "inv-1:billing:1", b'"I was charged twice"'),
        ("billing", "chat", "bob", "inv-1:billing:2", b'"I was charged twice"'),
    ]


async def test_a_retried_attempt_asks_with_the_same_keys():
    _, desk = agents()
    alice = Object()
    for _ in range(2):
        await alice.invoke(desk.handlers["chat"].fn, "hello", invocation="inv-1")
    keys = [i for _, _, _, i, _ in alice.calls]
    assert keys == ["inv-1:billing:1", "inv-1:billing:2"] * 2


def test_a_chat_handler_takes_its_message_as_text_or_json():
    _, desk = agents()
    serde = desk.handlers["chat"].handler_io.input_serde
    assert serde.deserialize(b"Hi") == serde.deserialize(b'"Hi"') == "Hi"
    with pytest.raises(ValueError, match="has a handler chat"):
        desk.chat(lambda ctx, message: message)


def test_only_an_agent_has_clear_and_sync():
    billing, desk = agents()
    assert {"clear", "erase", "sync", "messages"} <= billing.handlers.keys()
    assert not {"clear", "erase", "sync"} & desk.handlers.keys()
    assert {"status", "stop", "steer"} <= desk.handlers.keys()

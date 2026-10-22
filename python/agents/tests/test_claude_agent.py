"""
The Claude Code agent (the Claude Agent SDK), on the scripted teller as the
Messages API that its CLI calls, over an object's state through crashes.
"""

import os

import pytest
import restate_fns as restate
from fakes import Bank, MessagesServer, Object, Teller
from restate_agents import claude_agent, tool


@pytest.fixture(autouse=True)
def clean_environment(monkeypatch):
    """The CLI's environment: none of a Claude Code session that runs the tests, and none of a real API."""
    for name in list(os.environ):
        if name.startswith("CLAUDE") or name.startswith("ANTHROPIC"):
            monkeypatch.delenv(name)


@pytest.fixture
def server():
    """The teller, as the Messages API."""
    server = MessagesServer(Teller()).start()
    yield server
    server.stop()


def api(server: MessagesServer) -> dict:
    """The options that point Claude Code at the teller."""
    return {"env": {"ANTHROPIC_BASE_URL": server.url, "ANTHROPIC_API_KEY": "sk-teller"}}


def world() -> tuple[Bank, Object]:
    """A bank, and alice's object, whose process the bank serves while it is alive."""
    bank, alice = Bank(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, alice


def agent(bank: Bank, server: MessagesServer, **options):
    obj = claude_agent.agent("teller", tools=[bank.restate_tool()], options=api(server), **options)
    return obj.handlers["chat"].fn, obj.handlers["messages"].fn


async def test_it_answers_and_keeps_the_conversation(server):
    bank, alice = world()
    chat, messages = agent(bank, server)
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    # The second message's request has the first, from the session the CLI resumed from the object's database.
    assert len(server.requests[-1]["messages"]) > 3
    page = await alice.invoke(messages, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"]
    [call] = page["messages"][3]["parts"]
    assert call["type"] == "tool" and call["name"] == "transfer" and call["output"] == "moved 5 from 1 to 2"
    assert page["messages"][4]["parts"] == [{"type": "text", "text": "Done: moved 5 from 1 to 2"}]


async def test_a_message_is_answered_once_per_invocation(server):
    bank, alice = world()
    chat, _ = agent(bank, server)
    first = await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1")
    asked = server.teller.calls
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1") == first
    assert server.teller.calls == asked
    assert len(bank.moves) == 1


async def test_a_crash_in_a_tool_carries_the_turn_on_with_the_calls_id(server):
    bank, alice = world()
    chat, messages = agent(bank, server)
    bank.crash_in_call = alice.crash
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    # The tool ran twice, with one call id: the model's second call, with the same input, is the first one.
    assert bank.calls == 2
    assert list(bank.moves.values()) == [(1, 2, 5)]


async def test_a_terminal_error_of_a_tool_is_its_answer_to_the_model(server):
    @tool
    async def refuse(reason: str) -> str:
        """Refuse."""
        raise restate.TerminalError(f"no: {reason}")

    obj = claude_agent.agent("teller", tools=[refuse], options=api(server))
    assert await Object().invoke(obj.handlers["chat"].fn, "refuse politely") == "Done: no: politely"


async def test_any_other_error_of_a_tool_fails_the_attempt(server):
    @tool(name="refuse")
    async def flaky(reason: str) -> str:
        """Fail."""
        raise ConnectionError("unavailable")

    obj = claude_agent.agent("teller", tools=[flaky], options=api(server))
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


async def test_the_system_prompt_is_added_to_claude_codes(server):
    obj = claude_agent.agent("teller", system="You are a teller.", options=api(server))
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    [system] = [s for s in server.teller.systems if "You are a teller." in s]
    assert len(system) > len("You are a teller.")


async def test_a_steered_message_reaches_the_model_after_the_next_tool_call(server):
    bank, alice = world()
    chat, _ = agent(bank, server)
    alice.steer("only to account 2, please")
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Steered: only to account 2, please"
    [(handler, closing)] = alice.sends
    assert (handler, closing.taken, closing.cancelled) == ("close", ["turn:steer:1"], False)


async def test_a_cancelled_turn_ends_and_the_next_message_starts_afresh(server):
    @tool
    async def wait_for_approval() -> str:
        """Wait for an approval."""
        raise restate.TerminalError("cancelled", 409)  # as Restate's cancellation reaches a wait

    obj = claude_agent.agent("support", tools=[wait_for_approval], options=api(server))
    chat = obj.handlers["chat"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"


async def test_reset_starts_a_new_session_with_the_handoff(server):
    obj = claude_agent.agent("teller", options=api(server))

    @obj.handler(message=True)
    async def reset(ctx, handoff: str) -> None:
        await claude_agent.reset(ctx, handoff)

    alice = Object()
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    await alice.invoke(chat, "hello")
    await alice.invoke(obj.handlers["reset"].fn, "Alice likes short answers")
    assert await alice.invoke(chat, "hi") == "You said: Before this conversation: Alice likes short answers\n\nhi"
    page = await alice.invoke(messages, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant"]


def test_a_model_of_another_provider_fails_when_the_agent_is_defined():
    with pytest.raises(ValueError, match="anthropic/<model-id>"):
        claude_agent.agent("assistant", model="openai/gpt-5")


async def test_a_crash_at_any_state_request_moves_money_once(server):
    probe = Object()
    chat, _ = agent(Bank(), server)
    await probe.invoke(chat, "hello")
    before = probe.requests
    await probe.invoke(chat, "move 5 from 1 to 2")
    requests = probe.requests - before
    assert requests > 5

    for at in range(1, requests + 1):
        bank, alice = world()
        chat, messages = agent(bank, server)
        assert await alice.invoke(chat, "hello") == "You said: hello"
        alice.crash_at = alice.requests + at
        answer = await alice.invoke(chat, "move 5 from 1 to 2")
        assert answer == "Done: moved 5 from 1 to 2", f"crash at request {at}"
        assert list(bank.moves.values()) == [(1, 2, 5)], f"crash at request {at}: {bank.moves}"


async def test_a_wrong_api_key_is_a_terminal_error(server):
    env = {
        "ANTHROPIC_BASE_URL": server.url,
        "ANTHROPIC_API_KEY": MessagesServer.WRONG_KEY,
        "CLAUDE_CODE_MAX_RETRIES": "0",
    }
    obj = claude_agent.agent("assistant", options={"env": env})
    with pytest.raises(restate.TerminalError, match="check the agent's model"):
        await Object().run(obj.handlers["chat"].fn, "hello")

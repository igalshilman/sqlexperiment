"""The LangGraph agent, on the scripted teller, over an object's state through crashes."""

import pytest
import restate
from fakes import Bank, Object, Teller
from restate_agents import langgraph


def world() -> tuple[Bank, Teller, Object]:
    """A bank, a teller, and alice's object, whose process the bank serves while it is alive."""
    bank, teller, alice = Bank(), Teller(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, teller, alice


def agent(bank: Bank, teller: Teller, **options):
    obj = langgraph.agent("teller", model=teller, tools=[bank.tool()], **options)
    return obj.handlers["chat"].fn, obj.handlers["messages"].fn


async def test_it_answers_and_keeps_the_thread():
    bank, teller = Bank(), Teller()
    chat, messages = agent(bank, teller)
    alice = Object()
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    [(src, dst, amount)] = bank.moves.values()
    assert (src, dst, amount) == (1, 2, 5)
    page = await alice.invoke(messages, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"]
    [tool] = page["messages"][3]["parts"]
    assert tool["type"] == "tool" and tool["name"] == "transfer" and tool["output"] == "moved 5 from 1 to 2"
    assert page["messages"][4]["parts"] == [{"type": "text", "text": "Done: moved 5 from 1 to 2"}]
    assert not page["more"]
    later = await alice.invoke(messages, {"after": 2})
    assert [m["id"] for m in later["messages"]] == [3, 4]


async def test_a_message_is_answered_once_per_invocation():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    first = await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1")
    asked = teller.calls
    # The same invocation again, such as a retried attempt after the answer: the same answer, nothing asked or moved.
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1") == first
    assert teller.calls == asked
    assert len(bank.moves) == 1


async def test_a_crash_in_a_tool_resumes_it_with_its_call_id_without_asking_the_model_again():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    bank.crash_in_call = alice.crash
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    # The tool ran twice, with one call id; the model was asked once for the call, and once for the answer.
    assert bank.calls == 2
    assert len(bank.moves) == 1
    assert teller.calls == 2


async def test_a_crash_at_any_state_request_moves_money_once():
    # How many state requests a message makes, without crashes.
    probe = Object()
    chat, _ = agent(Bank(), Teller())
    await probe.invoke(chat, "hello")
    before = probe.requests
    await probe.invoke(chat, "move 5 from 1 to 2")
    requests = probe.requests - before
    assert requests > 10

    for at in range(1, requests + 1):
        bank, teller, alice = world()
        chat, messages = agent(bank, teller)
        assert await alice.invoke(chat, "hello") == "You said: hello"
        alice.crash_at = alice.requests + at
        answer = await alice.invoke(chat, "move 5 from 1 to 2")
        assert answer == "Done: moved 5 from 1 to 2", f"crash at request {at}"
        assert list(bank.moves.values()) == [(1, 2, 5)], f"crash at request {at}: {bank.moves}"
        # The conversation has the message once, and its answer once.
        page = await alice.invoke(messages, None)
        users = [m for m in page["messages"] if m["role"] == "user"]
        assert len(users) == 2, f"crash at request {at}: {page}"


async def test_an_interrupted_run_is_finished_before_the_next_message():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    bank.crash_in_call = alice.crash
    from fakes import Crash

    with pytest.raises(Crash):
        await alice.run(chat, "move 5 from 1 to 2", invocation="lost")
    # The next message (another invocation) finishes the interrupted transfer first, then answers.
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    # The interrupted invocation, retried, finds its answer.
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="lost") == "Done: moved 5 from 1 to 2"
    assert len(bank.moves) == 1


async def test_the_model_gets_the_system_prompt():
    teller = Teller()
    chat, _ = agent(Bank(), teller, system="You are a teller.")
    await Object().invoke(chat, "hello")
    assert teller.systems == ["You are a teller."]


async def test_a_terminal_error_of_a_tool_is_its_answer_to_the_model():
    from langchain_core.tools import tool

    @tool
    async def refuse(reason: str) -> str:
        """Refuse."""
        raise restate.TerminalError(f"no: {reason}")

    @tool
    async def flaky(reason: str) -> str:
        """Fail."""
        raise ConnectionError("unavailable")

    obj = langgraph.agent("teller", model=Teller(), tools=[refuse])
    answer = await Object().invoke(obj.handlers["chat"].fn, "refuse politely")
    assert answer == "Done: no: politely"
    # Any other error fails the attempt, for Restate to retry.
    flaky.name = "refuse"
    obj = langgraph.agent("teller", model=Teller(), tools=[flaky])
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


def test_a_model_that_is_not_anthropics_is_refused_early():
    with pytest.raises(ValueError, match="not anthropic's"):
        langgraph.agent("teller", model="openai/gpt-6-luna")


async def test_a_missing_api_key_is_a_terminal_error(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("MODEL", raising=False)
    obj = langgraph.agent("assistant")
    with pytest.raises(restate.TerminalError, match="check the agent's model"):
        await Object().run(obj.handlers["chat"].fn, "hello")

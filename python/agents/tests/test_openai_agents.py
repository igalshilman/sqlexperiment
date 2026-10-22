"""The OpenAI Agents SDK agent, on the scripted teller as an SDK model, over an object's state through crashes."""

import pytest
import restate_fns as restate
from agents import Agent, function_tool
from fakes import AgentsTeller, Bank, Crash, Object, Teller
from pydantic import BaseModel
from restate_agents import openai_agents, tool


@pytest.fixture(autouse=True)
def no_tracing(monkeypatch):
    """No traces sent to OpenAI: the tests have no key."""
    monkeypatch.setenv("OPENAI_AGENTS_DISABLE_TRACING", "1")


def world() -> tuple[Bank, Teller, Object]:
    """A bank, a teller, and alice's object, whose process the bank serves while it is alive."""
    bank, teller, alice = Bank(), Teller(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, teller, alice


def agent(bank: Bank, teller: Teller, **options):
    obj = openai_agents.agent("teller", model=AgentsTeller(teller), tools=[bank.restate_tool()], **options)
    return obj.handlers["chat"].fn, obj.handlers["messages"].fn


async def test_it_answers_and_keeps_the_conversation():
    bank, teller, alice = world()
    chat, messages = agent(bank, teller)
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    page = await alice.invoke(messages, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"]
    [call] = page["messages"][3]["parts"]
    assert call["type"] == "tool" and call["name"] == "transfer" and call["output"] == "moved 5 from 1 to 2"
    assert call["input"] == {"src": 1, "dst": 2, "amount": 5}
    assert page["messages"][4]["parts"] == [{"type": "text", "text": "Done: moved 5 from 1 to 2"}]


async def test_a_message_is_answered_once_per_invocation():
    bank, teller, alice = world()
    chat, _ = agent(bank, teller)
    first = await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1")
    asked = teller.calls
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1") == first
    assert teller.calls == asked
    assert len(bank.moves) == 1


async def test_a_crash_in_a_tool_resumes_it_with_its_call_id_without_asking_the_model_again():
    bank, teller, alice = world()
    chat, _ = agent(bank, teller)
    bank.crash_in_call = alice.crash
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    # The tool ran twice, with one call id; the model was asked once for the call, and once for the answer.
    assert bank.calls == 2
    assert len(bank.moves) == 1
    assert teller.calls == 2


async def test_a_crash_at_any_state_request_moves_money_once():
    probe = Object()
    chat, _ = agent(Bank(), Teller())
    await probe.invoke(chat, "hello")
    before = probe.requests
    await probe.invoke(chat, "move 5 from 1 to 2")
    requests = probe.requests - before
    assert requests > 5

    for at in range(1, requests + 1):
        bank, teller, alice = world()
        chat, messages = agent(bank, teller)
        assert await alice.invoke(chat, "hello") == "You said: hello"
        alice.crash_at = alice.requests + at
        answer = await alice.invoke(chat, "move 5 from 1 to 2")
        assert answer == "Done: moved 5 from 1 to 2", f"crash at request {at}"
        assert list(bank.moves.values()) == [(1, 2, 5)], f"crash at request {at}: {bank.moves}"
        page = await alice.invoke(messages, None)
        assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"], (
            f"crash at request {at}: {page}"
        )


async def test_an_interrupted_run_is_finished_before_the_next_message():
    bank, teller, alice = world()
    chat, _ = agent(bank, teller)
    bank.crash_in_call = alice.crash
    with pytest.raises(Crash):
        await alice.run(chat, "move 5 from 1 to 2", invocation="lost")
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="lost") == "Done: moved 5 from 1 to 2"
    assert len(bank.moves) == 1


async def test_the_model_gets_the_instructions():
    teller = Teller()
    obj = openai_agents.agent("teller", model=AgentsTeller(teller), instructions="You are a teller.")
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    assert teller.systems == ["You are a teller."]


def test_a_model_of_another_provider_fails_when_the_agent_is_defined():
    with pytest.raises(ValueError, match="openai/<model-id>"):
        openai_agents.agent("assistant", model="anthropic/claude-sonnet-5-5")


async def test_a_terminal_error_of_a_tool_is_its_answer_to_the_model():
    @tool
    async def refuse(reason: str) -> str:
        """Refuse."""
        raise restate.TerminalError(f"no: {reason}")

    @tool(name="refuse")
    async def flaky(reason: str) -> str:
        """Fail."""
        raise ConnectionError("unavailable")

    obj = openai_agents.agent("teller", model=AgentsTeller(Teller()), tools=[refuse])
    assert await Object().invoke(obj.handlers["chat"].fn, "refuse politely") == "Done: no: politely"
    # Any other error fails the attempt, for Restate to retry.
    obj = openai_agents.agent("teller", model=AgentsTeller(Teller()), tools=[flaky])
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


async def test_the_sdks_tools_work_as_they_are():
    @function_tool
    def transfer(src: int, dst: int, amount: int) -> str:
        """Move money."""
        return f"moved {amount}"

    obj = openai_agents.agent("teller", model=AgentsTeller(Teller()), tools=[transfer])
    assert await Object().invoke(obj.handlers["chat"].fn, "move 5 from 1 to 2") == "Done: moved 5"


async def test_a_steered_message_joins_the_turn_at_its_next_tool_call():
    bank, teller, alice = world()
    chat, messages = agent(bank, teller)
    alice.steer("only to account 2, please")
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Steered: only to account 2, please"
    page = await alice.invoke(messages, None)
    assert [m["parts"][0]["text"] for m in page["messages"] if m["role"] == "user"] == [
        "move 5 from 1 to 2",
        "only to account 2, please",
    ]
    [(handler, closing)] = alice.sends
    assert (handler, closing.taken, closing.cancelled) == ("close", ["turn:steer:1"], False)


async def test_a_cancelled_turn_ends_and_the_next_message_starts_afresh():
    @tool
    async def wait_for_approval() -> str:
        """Wait for an approval."""
        raise restate.TerminalError("cancelled", 409)  # as Restate's cancellation reaches a wait

    obj = openai_agents.agent("support", model=AgentsTeller(Teller()), tools=[wait_for_approval])
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"
    page = await alice.invoke(messages, None)
    [cancelled] = [m for m in page["messages"] if m["role"] == "assistant" and m["parts"][0]["type"] == "tool"]
    assert cancelled["parts"][0]["output"] == openai_agents.CANCELLED


class Plan(BaseModel):
    days: int


async def test_an_agent_of_your_own_and_its_typed_output_answered_once():
    @function_tool
    def draft() -> str:
        """Draft the plan."""
        return '{"days": 3}'

    teller = Teller()  # it answers with the tool's result: the plan, as JSON
    planner = Agent(name="planner", model=AgentsTeller(teller), tools=[draft], output_type=Plan)
    obj = openai_agents.agent("trips", Agent(name="trips", model=AgentsTeller(teller)))

    @obj.handler
    async def plan(ctx) -> Plan:
        return await openai_agents.prompt(ctx, planner, "use draft {}", thread="plans")

    alice = Object()
    assert await alice.invoke(obj.handlers["plan"].fn, invocation="p1") == Plan(days=3)
    asked = teller.calls
    assert await alice.invoke(obj.handlers["plan"].fn, invocation="p1") == Plan(days=3)
    assert teller.calls == asked

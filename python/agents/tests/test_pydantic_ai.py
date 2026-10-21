"""The Pydantic AI agent, on the scripted teller, over an object's state through crashes."""

import pytest
import restate_fns as restate
from fakes import Bank, Crash, Object, Teller, pydantic_ai_model
from pydantic import BaseModel
from pydantic_ai import Agent
from pydantic_ai.messages import ModelMessage, ModelResponse, TextPart, ToolCallPart
from pydantic_ai.models.function import AgentInfo, FunctionModel
from restate_agents import pydantic_ai, tool


def world() -> tuple[Bank, Teller, Object]:
    """A bank, a teller, and alice's object, whose process the bank serves while it is alive."""
    bank, teller, alice = Bank(), Teller(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, teller, alice


def agent(bank: Bank, teller: Teller, **options):
    obj = pydantic_ai.agent("teller", model=pydantic_ai_model(teller), tools=[bank.restate_tool()], **options)
    return obj.handlers["chat"].fn, obj.handlers["messages"].fn


async def test_it_answers_and_keeps_the_conversation():
    bank, teller = Bank(), Teller()
    chat, messages = agent(bank, teller)
    alice = Object()
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    page = await alice.invoke(messages, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"]
    [tool] = page["messages"][3]["parts"]
    assert tool["type"] == "tool" and tool["name"] == "transfer" and tool["output"] == "moved 5 from 1 to 2"
    assert page["messages"][4]["parts"] == [{"type": "text", "text": "Done: moved 5 from 1 to 2"}]


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
    assert requests > 5

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
        assert [m["role"] for m in page["messages"]] == ["user", "assistant", "user", "assistant", "assistant"], (
            f"crash at request {at}: {page}"
        )


async def test_an_interrupted_run_is_finished_before_the_next_message():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    bank.crash_in_call = alice.crash
    with pytest.raises(Crash):
        await alice.run(chat, "move 5 from 1 to 2", invocation="lost")
    # The next message (another invocation) finishes the interrupted transfer first, then answers.
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    # The interrupted invocation, retried, finds its answer.
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="lost") == "Done: moved 5 from 1 to 2"
    assert len(bank.moves) == 1


async def test_the_model_gets_the_instructions():
    teller = Teller()
    chat, _ = agent(Bank(), teller, instructions="You are a teller.")
    await Object().invoke(chat, "hello")
    assert teller.systems == ["You are a teller."]


async def test_a_terminal_error_of_a_tool_is_its_answer_to_the_model():
    @tool
    async def refuse(reason: str) -> str:
        """Refuse."""
        raise restate.TerminalError(f"no: {reason}")

    @tool(name="refuse")
    async def flaky(reason: str) -> str:
        """Fail."""
        raise ConnectionError("unavailable")

    obj = pydantic_ai.agent("teller", model=pydantic_ai_model(Teller()), tools=[refuse])
    assert await Object().invoke(obj.handlers["chat"].fn, "refuse politely") == "Done: no: politely"
    # Any other error fails the attempt, for Restate to retry.
    obj = pydantic_ai.agent("teller", model=pydantic_ai_model(Teller()), tools=[flaky])
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


async def test_a_missing_api_key_is_a_terminal_error(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("MODEL", raising=False)
    obj = pydantic_ai.agent("assistant")
    with pytest.raises(restate.TerminalError, match="check the agent's model"):
        await Object().run(obj.handlers["chat"].fn, "hello")


async def test_a_tool_gets_the_context_and_its_call_id():
    seen = []

    @tool
    async def transfer(ctx, call_id, src: int, dst: int, amount: int) -> str:
        """Move money."""
        seen.append((ctx.key(), call_id, src, dst, amount))
        return "moved"

    obj = pydantic_ai.agent("teller", model=pydantic_ai_model(Teller()), tools=[transfer])
    assert await Object("bob").invoke(obj.handlers["chat"].fn, "move 5 from 1 to 2") == "Done: moved"
    [(key, call_id, src, dst, amount)] = seen
    assert (key, src, dst, amount) == ("bob", 1, 2, 5) and call_id


async def test_pydantic_ai_tool_functions_work_as_they_are():
    def transfer(src: int, dst: int, amount: int) -> str:
        """Move money."""
        return f"moved {amount}"

    obj = pydantic_ai.agent("teller", model=pydantic_ai_model(Teller()), tools=[transfer])
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
    assert "restate-agents.turn" not in alice.values


async def test_a_steered_message_is_taken_once_through_a_crash():
    bank, teller, alice = world()
    chat, messages = agent(bank, teller)
    alice.steer("only to account 2, please")
    asked = 0

    def model(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        nonlocal asked
        asked += 1
        if asked == 2:
            alice.crash()  # once the steered message is stored, before the answer
            raise ConnectionError("the process died")
        return pydantic_ai_model(teller).function(messages, info)

    obj = pydantic_ai.agent("teller", model=FunctionModel(model), tools=[bank.restate_tool()])
    assert await alice.invoke(obj.handlers["chat"].fn, "move 5 from 1 to 2") == "Steered: only to account 2, please"
    [(_, closing)] = alice.sends
    assert closing.taken == ["turn:steer:1"]
    page = await alice.invoke(obj.handlers["messages"].fn, None)
    assert len([m for m in page["messages"] if m["role"] == "user"]) == 2


async def test_a_turn_without_a_tool_call_leaves_its_steered_messages_to_be_sent():
    bank, teller, alice = world()
    chat, _ = agent(bank, teller)
    alice.steer("and then?")
    assert await alice.invoke(chat, "hello") == "You said: hello"
    [(_, closing)] = alice.sends
    assert closing.taken == []


async def test_a_cancelled_turn_ends_and_the_next_message_starts_afresh():
    @tool
    async def wait_for_approval() -> str:
        """Wait for an approval."""
        raise restate.TerminalError("cancelled", 409)  # as Restate's cancellation reaches a wait

    obj = pydantic_ai.agent("support", model=pydantic_ai_model(Teller()), tools=[wait_for_approval])
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"
    page = await alice.invoke(messages, None)
    [cancelled] = [m for m in page["messages"] if m["role"] == "assistant" and m["parts"][0]["type"] == "tool"]
    assert cancelled["parts"][0]["output"] == pydantic_ai.CANCELLED
    assert [m["parts"][-1].get("text") for m in page["messages"]][-3:] == [
        pydantic_ai.CANCELLED,
        "hello",
        "You said: hello",
    ]


def shouter(runs: list[str]) -> Agent:
    """An agent that answers the last message in capitals."""

    def shout(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        text = messages[-1].parts[-1].content
        runs.append(text)
        return ModelResponse(parts=[TextPart(text.upper())])

    return Agent(FunctionModel(shout))


async def test_an_agent_of_your_own():
    runs: list[str] = []
    chat = pydantic_ai.agent("shouter", shouter(runs)).handlers["chat"].fn
    alice = Object()
    assert await alice.invoke(chat, "hi", invocation="i1") == "HI"
    # The same invocation, again: answered from the conversation, without asking the model.
    assert await alice.invoke(chat, "hi", invocation="i1") == "HI"
    assert runs == ["hi"]


async def test_prompts_of_a_handler_of_your_own_are_answered_once_each():
    runs: list[str] = []
    shouting = shouter(runs)
    obj = pydantic_ai.agent("shouter", shouting)

    @obj.handler
    async def twice(ctx, text: str) -> list[str]:
        return [await pydantic_ai.prompt(ctx, shouting, text), await pydantic_ai.prompt(ctx, shouting, f"{text}!")]

    alice = Object()
    handler = obj.handlers["twice"].fn
    assert await alice.invoke(handler, "hi", invocation="i1") == ["HI", "HI!"]
    assert await alice.invoke(handler, "hi", invocation="i1") == ["HI", "HI!"]
    assert runs == ["hi", "hi!"]


async def test_a_structured_output_is_answered_once():
    class Verdict(BaseModel):
        approved: bool

    asked = 0

    def judge(messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        nonlocal asked
        asked += 1
        return ModelResponse(parts=[ToolCallPart(info.output_tools[0].name, {"approved": True})])

    judging = Agent(FunctionModel(judge), output_type=Verdict)
    obj = pydantic_ai.agent("judge", judging)

    @obj.handler
    async def review(ctx, text: str) -> bool:
        return (await pydantic_ai.prompt(ctx, judging, text)).approved

    alice = Object()
    assert await alice.invoke(obj.handlers["review"].fn, "ship it?", invocation="i1") is True
    assert await alice.invoke(obj.handlers["review"].fn, "ship it?", invocation="i1") is True
    assert asked == 1


async def test_an_agent_of_your_own_gets_restates_tools_as_a_toolset():
    bank = Bank()
    own = Agent(pydantic_ai_model(Teller()), toolsets=[pydantic_ai.toolset([bank.restate_tool()])])
    chat = pydantic_ai.agent("teller", own).handlers["chat"].fn
    assert await Object().invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
    assert list(bank.moves.values()) == [(1, 2, 5)]


def test_an_agent_takes_an_agent_or_the_usual_options():
    with pytest.raises(TypeError, match="not both"):
        pydantic_ai.agent("shouter", shouter([]), instructions="Shout.")

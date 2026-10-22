"""The LiteLLM agent, on the scripted teller as a LiteLLM provider, over an object's state through crashes."""

import pytest
import restate_fns as restate
from fakes import Bank, Crash, LiteLLMTeller, Object, Teller
from restate_agents import litellm, tool

MODEL = "teller/teller"


def world() -> tuple[Bank, Teller, Object]:
    """A bank, a teller, and alice's object, whose process the bank serves while it is alive."""
    bank, teller, alice = Bank(), Teller(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, teller, alice


def agent(bank: Bank, teller: Teller, **options):
    LiteLLMTeller(teller).register()
    obj = litellm.agent("teller", model=MODEL, tools=[bank.restate_tool()], **options)
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
    [call] = page["messages"][3]["parts"]
    assert call["type"] == "tool" and call["name"] == "transfer" and call["output"] == "moved 5 from 1 to 2"
    assert call["input"] == {"src": 1, "dst": 2, "amount": 5}
    assert page["messages"][4]["parts"] == [{"type": "text", "text": "Done: moved 5 from 1 to 2"}]


async def test_a_message_is_answered_once_per_invocation():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    first = await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1")
    asked = teller.calls
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="inv-1") == first
    assert teller.calls == asked
    assert len(bank.moves) == 1


async def test_a_crash_in_a_tool_resumes_it_with_its_call_id_without_asking_the_model_again():
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    bank.crash_in_call = alice.crash
    assert await alice.invoke(chat, "move 5 from 1 to 2") == "Done: moved 5 from 1 to 2"
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
    bank, teller = Bank(), Teller()
    chat, _ = agent(bank, teller)
    alice = Object()
    bank.crash_in_call = alice.crash
    with pytest.raises(Crash):
        await alice.run(chat, "move 5 from 1 to 2", invocation="lost")
    assert await alice.invoke(chat, "hello") == "You said: hello"
    assert list(bank.moves.values()) == [(1, 2, 5)]
    assert await alice.invoke(chat, "move 5 from 1 to 2", invocation="lost") == "Done: moved 5 from 1 to 2"
    assert len(bank.moves) == 1


async def test_the_model_gets_the_system_prompt_the_tools_and_more_of_the_request():
    teller = Teller()
    model = LiteLLMTeller(teller).register()
    obj = litellm.agent("teller", model=MODEL, system="You are a teller.", request={"temperature": 0.25})
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    assert teller.systems == ["You are a teller."]
    [request] = model.requests
    assert request["model"] == "teller" and request["temperature"] == 0.25 and "tools" not in request

    obj = litellm.agent("teller", model=MODEL, tools=[Bank().restate_tool()])
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    [transfer] = model.requests[-1]["tools"]
    assert transfer["function"]["name"] == "transfer"
    assert set(transfer["function"]["parameters"]["properties"]) == {"src", "dst", "amount"}


def test_a_model_litellm_does_not_know_fails_when_the_agent_is_defined():
    with pytest.raises(ValueError, match="provider/model-id"):
        litellm.agent("assistant", model="llama4")


async def test_options_for_each_invocation():
    teller = Teller()
    LiteLLMTeller(teller).register()
    obj = litellm.agent("teller", model=lambda ctx: MODEL, system=lambda ctx: f"You serve {ctx.key()}.")
    await Object("bob").invoke(obj.handlers["chat"].fn, "hello")
    assert teller.systems == ["You serve bob."]


async def test_a_terminal_error_of_a_tool_is_its_answer_to_the_model():
    @tool
    async def refuse(reason: str) -> str:
        """Refuse."""
        raise restate.TerminalError(f"no: {reason}")

    @tool(name="refuse")
    async def flaky(reason: str) -> str:
        """Fail."""
        raise ConnectionError("unavailable")

    LiteLLMTeller(Teller()).register()
    obj = litellm.agent("teller", model=MODEL, tools=[refuse])
    assert await Object().invoke(obj.handlers["chat"].fn, "refuse politely") == "Done: no: politely"
    obj = litellm.agent("teller", model=MODEL, tools=[flaky])
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


async def test_a_missing_api_key_is_a_terminal_error(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("MODEL", raising=False)
    obj = litellm.agent("assistant")
    with pytest.raises(restate.TerminalError, match="check the agent's model"):
        await Object().run(obj.handlers["chat"].fn, "hello")


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

    LiteLLMTeller(Teller()).register()
    obj = litellm.agent("support", model=MODEL, tools=[wait_for_approval])
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"
    page = await alice.invoke(messages, None)
    [cancelled] = [m for m in page["messages"] if m["role"] == "assistant" and m["parts"][0]["type"] == "tool"]
    assert cancelled["parts"][0]["output"] == litellm.CANCELLED
    assert [m["parts"][-1].get("text") for m in page["messages"]][-3:] == [
        litellm.CANCELLED,
        "hello",
        "You said: hello",
    ]


async def test_prompts_compact_and_reset_of_handlers_of_your_own():
    teller = Teller()
    model = LiteLLMTeller(teller).register()
    obj = litellm.agent("teller", model=MODEL)

    @obj.handler
    async def twice(ctx, text: str) -> list[str]:
        return [
            await litellm.prompt(ctx, text, model=MODEL),
            await litellm.prompt(ctx, f"{text}!", model=MODEL, thread="other"),
        ]

    @obj.handler(message=True)
    async def compact(ctx, instructions: str) -> str:
        return await litellm.compact(ctx, instructions, model=MODEL)

    alice = Object()
    handler = obj.handlers["twice"].fn
    assert await alice.invoke(handler, "hi", invocation="i1") == ["You said: hi", "You said: hi!"]
    assert await alice.invoke(handler, "hi", invocation="i1") == ["You said: hi", "You said: hi!"]
    assert teller.calls == 2
    assert [len(r["messages"]) for r in model.requests] == [1, 1]

    summary = await alice.invoke(obj.handlers["compact"].fn, "Keep it short")
    assert summary.startswith("You said: Summarize our conversation")
    page = await alice.invoke(obj.handlers["messages"].fn, None)
    assert [m["role"] for m in page["messages"]] == ["user", "assistant"]
    assert page["messages"][0]["parts"][0]["text"].endswith(summary)

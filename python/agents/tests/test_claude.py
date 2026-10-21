"""The Claude agent, on the scripted teller as an Anthropic client, over an object's state through crashes."""

import pytest
import restate_fns as restate
from anthropic import beta_async_tool
from fakes import Bank, ClaudeClient, Crash, Object, Teller
from restate_agents import claude, tool


def world() -> tuple[Bank, Teller, Object]:
    """A bank, a teller, and alice's object, whose process the bank serves while it is alive."""
    bank, teller, alice = Bank(), Teller(), Object()
    bank.alive = lambda: not alice.crashed
    return bank, teller, alice


def agent(bank: Bank, teller: Teller, **options):
    obj = claude.agent("teller", client=ClaudeClient(teller), tools=[bank.restate_tool()], **options)
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


async def test_the_model_gets_the_system_prompt_model_and_tools():
    teller = Teller()
    client = ClaudeClient(teller)
    obj = claude.agent("teller", model="anthropic/claude-opus-5-5", system="You are a teller.", client=client)
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    assert teller.systems == ["You are a teller."]
    [request] = client.requests
    assert request["model"] == "claude-opus-5-5" and "tools" not in request


async def test_requests_are_the_apis_own_with_fallbacks_unless_given():
    client = ClaudeClient(Teller())
    obj = claude.agent("teller", client=client)
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    obj = claude.agent("teller", client=client, request={"output_config": {"effort": "low"}, "fallbacks": None})
    await Object().invoke(obj.handlers["chat"].fn, "hello")
    fallback, own = client.requests
    assert (fallback["max_tokens"], fallback["fallbacks"], fallback["betas"]) == (
        16000,
        "default",
        ["server-side-fallback-2026-07-01"],
    )
    assert own["output_config"] == {"effort": "low"} and own["fallbacks"] is None and "betas" not in own


async def test_a_declined_request_is_answered_so():
    client = ClaudeClient(Teller())
    create = client.create

    async def declining(**params):
        reply = await create(**params)
        return reply.model_copy(update={"content": [], "stop_reason": "refusal"})

    client.create = declining
    obj = claude.agent("teller", client=client)
    assert await Object().invoke(obj.handlers["chat"].fn, "hello") == claude.DECLINED


def test_a_model_of_another_provider_fails_when_the_agent_is_defined():
    with pytest.raises(ValueError, match="anthropic/<model-id>"):
        claude.agent("assistant", model="openai/gpt-6-luna")


async def test_options_for_each_invocation():
    teller = Teller()
    obj = claude.agent("teller", system=lambda ctx: f"You serve {ctx.key()}.", client=ClaudeClient(teller))
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

    obj = claude.agent("teller", client=ClaudeClient(Teller()), tools=[refuse])
    assert await Object().invoke(obj.handlers["chat"].fn, "refuse politely") == "Done: no: politely"
    # Any other error fails the attempt, for Restate to retry.
    obj = claude.agent("teller", client=ClaudeClient(Teller()), tools=[flaky])
    with pytest.raises(ConnectionError):
        await Object().run(obj.handlers["chat"].fn, "refuse politely")


async def test_a_missing_api_key_is_a_terminal_error(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("ANTHROPIC_AUTH_TOKEN", raising=False)
    monkeypatch.delenv("MODEL", raising=False)
    obj = claude.agent("assistant")
    with pytest.raises(restate.TerminalError, match="check the agent's model"):
        await Object().run(obj.handlers["chat"].fn, "hello")


async def test_a_tool_gets_the_context_and_its_call_id():
    seen = []

    @tool
    async def transfer(ctx, call_id, src: int, dst: int, amount: int) -> str:
        """Move money."""
        seen.append((ctx.key(), call_id, src, dst, amount))
        return "moved"

    obj = claude.agent("teller", client=ClaudeClient(Teller()), tools=[transfer])
    assert await Object("bob").invoke(obj.handlers["chat"].fn, "move 5 from 1 to 2") == "Done: moved"
    [(key, call_id, src, dst, amount)] = seen
    assert (key, src, dst, amount) == ("bob", 1, 2, 5) and call_id


async def test_the_sdks_tools_work_as_they_are():
    @beta_async_tool
    async def transfer(src: int, dst: int, amount: int) -> str:
        """Move money."""
        return f"moved {amount}"

    obj = claude.agent("teller", client=ClaudeClient(Teller()), tools=[transfer])
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
    client = ClaudeClient(teller)
    create = client.create

    async def crashing(**params):
        if len(client.requests) == 1:
            client.requests.append(params)
            alice.crash()  # once the steered message is stored, before the answer
            raise ConnectionError("the process died")
        return await create(**params)

    client.create = crashing
    obj = claude.agent("teller", client=client, tools=[bank.restate_tool()])
    alice.steer("only to account 2, please")
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

    obj = claude.agent("support", client=ClaudeClient(Teller()), tools=[wait_for_approval])
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"
    page = await alice.invoke(messages, None)
    [cancelled] = [m for m in page["messages"] if m["role"] == "assistant" and m["parts"][0]["type"] == "tool"]
    assert cancelled["parts"][0]["output"] == claude.CANCELLED
    assert [m["parts"][-1].get("text") for m in page["messages"]][-3:] == [claude.CANCELLED, "hello", "You said: hello"]


async def test_prompts_of_a_handler_of_your_own_are_answered_once_each():
    teller = Teller()
    client = ClaudeClient(teller)
    obj = claude.agent("teller", client=client)

    @obj.handler
    async def twice(ctx, text: str) -> list[str]:
        return [
            await claude.prompt(ctx, text, client=client),
            await claude.prompt(ctx, f"{text}!", client=client, thread="other"),
        ]

    alice = Object()
    handler = obj.handlers["twice"].fn
    assert await alice.invoke(handler, "hi", invocation="i1") == ["You said: hi", "You said: hi!"]
    assert await alice.invoke(handler, "hi", invocation="i1") == ["You said: hi", "You said: hi!"]
    assert teller.calls == 2
    # Each thread is a conversation of its own: the second prompt's request has only its own message.
    assert [len(r["messages"]) for r in client.requests] == [1, 1]


async def test_options_may_be_async_functions_of_the_context():
    teller = Teller()

    async def persona(ctx) -> str:
        return f"You serve {ctx.key()}."

    obj = claude.agent("teller", system=persona, client=ClaudeClient(teller))
    await Object("bob").invoke(obj.handlers["chat"].fn, "hello")
    assert teller.systems == ["You serve bob."]


def assistant(client: ClaudeClient):
    """An agent with compact and reset handlers of its own, which take their input as a message."""
    obj = claude.agent("assistant", client=client)

    @obj.handler(message=True)
    async def compact(ctx, instructions: str) -> str:
        return await claude.compact(ctx, instructions, client=client)

    @obj.handler(message=True)
    async def reset(ctx, handoff: str) -> None:
        await claude.reset(ctx, handoff)

    return obj


async def test_compact_summarizes_the_conversation_in_place_of_it():
    client = ClaudeClient(Teller())
    obj = assistant(client)
    alice = Object()
    await alice.invoke(obj.handlers["chat"].fn, "hello")
    summary = await alice.invoke(obj.handlers["compact"].fn, "Keep the greetings", invocation="c1")
    assert summary.startswith("You said: Summarize our conversation so far") and summary.endswith("Keep the greetings")
    page = await alice.invoke(obj.handlers["messages"].fn, None)
    assert [m["parts"][0]["text"] for m in page["messages"]] == [
        f"Our conversation so far, summarized:\n\n{summary}",
        "Understood: I will carry on from there.",
    ]
    # The same invocation again, such as a retried attempt: the same summary, the model not asked again.
    asked = len(client.requests)
    assert await alice.invoke(obj.handlers["compact"].fn, "Keep the greetings", invocation="c1") == summary
    assert len(client.requests) == asked
    # The next message carries on from the summary.
    await alice.invoke(obj.handlers["chat"].fn, "and now?")
    assert len(client.requests[-1]["messages"]) == 3


async def test_reset_starts_over_with_a_handoff_or_without():
    client = ClaudeClient(Teller())
    obj = assistant(client)
    alice = Object()
    await alice.invoke(obj.handlers["chat"].fn, "hello")
    await alice.invoke(obj.handlers["reset"].fn, "Alice likes short answers")
    page = await alice.invoke(obj.handlers["messages"].fn, None)
    assert page["messages"][0]["parts"][0]["text"] == "Before this conversation: Alice likes short answers"
    await alice.invoke(obj.handlers["reset"].fn, "")
    assert (await alice.invoke(obj.handlers["messages"].fn, None))["messages"] == []
    assert await alice.invoke(obj.handlers["chat"].fn, "hello") == "You said: hello"
    assert client.requests[-1]["messages"] == [{"role": "user", "content": "hello"}]


def test_a_handler_takes_its_input_as_a_message():
    obj = assistant(ClaudeClient(Teller()))
    serde = obj.handlers["compact"].handler_io.input_serde
    assert serde.deserialize(b"Keep it short") == serde.deserialize(b'"Keep it short"') == "Keep it short"

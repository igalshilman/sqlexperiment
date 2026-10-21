"""The LangGraph agent, on the scripted teller, over an object's state through crashes."""

import pytest
import restate_sql as restate
from fakes import Bank, Object, Teller
from langchain_core.messages import AIMessage
from langgraph.graph import START, MessagesState, StateGraph
from restate_agents import langgraph, tool


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


async def test_a_tool_gets_the_context_and_its_call_id():
    seen = []

    @tool
    async def transfer(ctx, call_id, src: int, dst: int, amount: int) -> str:
        """Move money."""
        seen.append((ctx.key(), call_id, src, dst, amount))
        return "moved"

    assert transfer.input.model_json_schema()["required"] == ["src", "dst", "amount"]
    obj = langgraph.agent("teller", model=Teller(), tools=[transfer])
    assert await Object("bob").invoke(obj.handlers["chat"].fn, "move 5 from 1 to 2") == "Done: moved"
    [(key, call_id, src, dst, amount)] = seen
    assert (key, src, dst, amount) == ("bob", 1, 2, 5) and call_id


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

    obj = langgraph.agent("support", model=Teller(), tools=[wait_for_approval])
    chat, messages = obj.handlers["chat"].fn, obj.handlers["messages"].fn
    alice = Object()
    with pytest.raises(restate.TerminalError, match="cancelled"):
        await alice.invoke(chat, "use wait_for_approval {}")
    [(_, closing)] = alice.sends
    assert closing.cancelled
    assert await alice.invoke(chat, "hello") == "You said: hello"
    page = await alice.invoke(messages, None)
    [cancelled] = [m for m in page["messages"] if m["role"] == "assistant" and m["parts"][0]["type"] == "tool"]
    assert cancelled["parts"][0]["output"] == langgraph.CANCELLED
    assert [m["parts"][-1].get("text") for m in page["messages"]][-3:] == [
        langgraph.CANCELLED,
        "hello",
        "You said: hello",
    ]


def shouting(runs: list[str]) -> StateGraph:
    """A graph of one node, which answers the last message in capitals."""

    async def shout(state: MessagesState) -> dict:
        runs.append(state["messages"][-1].content)
        return {"messages": [AIMessage(state["messages"][-1].content.upper())]}

    graph = StateGraph(MessagesState)
    graph.add_node("shout", shout)
    graph.add_edge(START, "shout")
    return graph


async def test_an_agent_on_a_graph_of_your_own():
    runs: list[str] = []
    obj = langgraph.agent("shouter", graph=shouting(runs))
    chat = obj.handlers["chat"].fn
    alice = Object()
    assert await alice.invoke(chat, "hi", invocation="i1") == "HI"
    # The same invocation, again: answered from the thread, without running the graph.
    assert await alice.invoke(chat, "hi", invocation="i1") == "HI"
    assert runs == ["hi"]


async def test_prompts_of_a_handler_of_your_own_are_answered_once_each():
    runs: list[str] = []
    graph = shouting(runs)
    obj = langgraph.agent("shouter", graph=graph)

    @obj.handler
    async def twice(ctx, text: str) -> list[str]:
        return [await langgraph.prompt(ctx, graph, text), await langgraph.prompt(ctx, graph, f"{text}!")]

    alice = Object()
    handler = obj.handlers["twice"].fn
    assert await alice.invoke(handler, "hi", invocation="i1") == ["HI", "HI!"]
    assert await alice.invoke(handler, "hi", invocation="i1") == ["HI", "HI!"]
    assert runs == ["hi", "hi!"]


def test_an_agent_takes_a_graph_or_the_usual_options():
    with pytest.raises(TypeError, match="not both"):
        langgraph.agent("shouter", graph=shouting([]), system="Shout.")


async def test_a_call_node_is_made_once_per_run_of_its_node_through_a_crash():
    from langgraph.graph import START
    from restate_agents.langgraph import nodes

    orders = restate.Service("orders")

    @orders.handler()
    async def status(ctx, order: str) -> str:
        """The status of an order."""

    class Order(MessagesState):
        status: str

    graph = StateGraph(Order)
    graph.add_node("status", nodes.call(status, input=lambda state: state["messages"][-1].content, update="status"))
    graph.add_node("answer", lambda state: {"messages": [AIMessage(state["status"])]})
    graph.add_edge(START, "status")
    graph.add_edge("status", "answer")
    chat = langgraph.agent("orders_desk", graph=graph).handlers["chat"].fn

    alice = Object()
    alice.answer = lambda handler, request: b'"delivered"'
    alice.crash_in_call = alice.crash
    assert await alice.invoke(chat, "103") == "delivered"
    # The call ran again after the crash, with the same idempotency key; the next message's call has its own.
    first, again = alice.calls
    assert first == again and first[:2] == ("orders", "status") and first[4] == b'"103"'
    assert await alice.invoke(chat, "104") == "delivered"
    assert alice.calls[2][3] != first[3]


def test_a_call_node_of_an_object_needs_its_key():
    from restate_agents.langgraph import nodes

    accounts = restate.VirtualObject("accounts")

    @accounts.handler()
    async def balance(ctx) -> int:
        """The balance."""

    with pytest.raises(TypeError, match="give the node its key"):
        nodes.call(balance)

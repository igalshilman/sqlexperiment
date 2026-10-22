"""
Restate's tools for agents (``call``, ``send``, ``workflow``, ``subagent`` and
``signal``), on the scripted model, against a real Restate server: the
support agents of services.py, on LangGraph, on Pydantic AI and on Claude.

    make concurrency-test-py   # gets restate-server first
"""

import json
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor

import pytest
from harness import ADMIN, INGRESS, call, request, wait_until


def key() -> str:
    return uuid.uuid4().hex[:12]


def use(*steps: tuple[str, dict]) -> str:
    """A prompt for the scripted model: these tool calls in turn, then their results joined by " / "."""
    return "use " + " then ".join(f"{tool} {json.dumps(args)}" for tool, args in steps)


REFUND = {"order": "103", "amount": 5}
SUPPORT = use(("orders_status", {"order": "103"}), ("wait_for_approval", REFUND), ("refund", REFUND))


@pytest.fixture(params=["support", "pydantic_support", "claude_support", "claude_agent_support", "codex_support"])
def agent(request) -> str:
    """The support agent: on LangGraph, on Pydantic AI, on Claude, on Claude Code, and on Codex."""
    return request.param


def pending(agent: str, k: str) -> list[dict]:
    status, waits = call(f"{agent}/{k}/pending")
    assert status == 200, waits
    return waits


def test_call_signal_and_workflow(agent):
    k = key()
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"{agent}/{k}/chat", SUPPORT, None, 60)
        wait_until(lambda: len(pending(agent, k)) == 1, timeout=20, what="the wait")
        [waiting] = pending(agent, k)
        assert (waiting["name"], waiting["input"]) == ("approval", REFUND)
        assert call(f"{agent}/{k}/signal", {"id": waiting["id"], "value": "approved by Kim"}) == (
            200,
            {"resolved": waiting["id"]},
        )
        assert answer.result() == (200, "order 103: delivered / approved by Kim / refunded 103")
    assert pending(agent, k) == []


def test_a_signal_no_one_waits_for_is_dropped_unless_kept(agent):
    dropped, kept = key(), key()
    assert call(f"{agent}/{dropped}/signal", {"name": "approval", "value": "early"}) == (200, {"resolved": None})
    assert call(f"{agent}/{kept}/signal", {"name": "approval", "value": "kept", "keep": True}) == (
        200,
        {"resolved": None},
    )
    assert call(f"{agent}/{kept}/chat", use(("wait_for_approval", REFUND))) == (200, "kept")
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"{agent}/{dropped}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(agent, dropped)) == 1, timeout=20, what="the wait")
        assert call(f"{agent}/{dropped}/signal", {"name": "approval", "value": "fresh"})[0] == 200
        assert answer.result() == (200, "fresh")


def test_a_cancelled_wait_is_forgotten(agent):
    k = key()
    status, body = request("POST", f"{INGRESS}/{agent}/{k}/chat/send", use(("wait_for_approval", REFUND)))
    assert status == 202, body
    wait_until(lambda: len(pending(agent, k)) == 1, timeout=20, what="the wait")
    status, _ = request("PATCH", f"{ADMIN}/invocations/{body['invocationId']}/cancel")
    assert status in (200, 202)
    wait_until(lambda: pending(agent, k) == [], timeout=20, what="the wait to be forgotten")


def test_a_handlers_terminal_error_is_the_tools_answer(agent):
    assert call(f"{agent}/{key()}/chat", use(("orders_status", {"order": "999"}))) == (200, "no order 999")


def test_a_subagent_continues_its_task_by_name(agent):
    k = key()
    task = use(("helper", {"name": "greet", "task": "hello"}))
    assert call(f"{agent}/{k}/chat", task) == (200, "You said: hello")
    status, page = call(f"helper/{k}.greet/messages")
    assert status == 200 and [m["parts"][0]["text"] for m in page["messages"]] == ["hello", "You said: hello"]


def test_send_does_not_wait(agent):
    k, other = key(), key()
    sent = use(("notes_add", {"key": other, "note": "call back"}))
    assert call(f"{agent}/{k}/chat", sent) == (200, f"sent to notes/{other}/add")
    wait_until(lambda: call(f"notes/{other}/list") == (200, ["call back"]), timeout=10, what="the note")


def test_status_and_stop_a_turn(agent):
    k = key()
    assert call(f"{agent}/{k}/status") == (200, {"answering": None, "waiting": []})
    assert call(f"{agent}/{k}/stop") == (200, {"stopped": None})
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"{agent}/{k}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(agent, k)) == 1, timeout=20, what="the wait")
        status, now = call(f"{agent}/{k}/status")
        assert status == 200 and now["answering"]["invocationId"].startswith("inv_") and len(now["waiting"]) == 1
        assert call(f"{agent}/{k}/stop") == (200, {"stopped": now["answering"]["invocationId"]})
        status, body = answer.result()
        assert status == 409, body
    wait_until(lambda: call(f"{agent}/{k}/status") == (200, {"answering": None, "waiting": []}), timeout=20)
    # The next message starts afresh.
    assert call(f"{agent}/{k}/chat", "hello") == (200, "You said: hello")


def test_steer_a_turn_at_its_next_tool_call(agent):
    k = key()
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"{agent}/{k}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(agent, k)) == 1, timeout=20, what="the wait")
        status, steered = call(f"{agent}/{k}/steer", "Only Monday, please")
        assert status == 200 and steered["steered"], steered
        assert call(f"{agent}/{k}/signal", {"name": "approval", "value": "approved"})[0] == 200
        assert answer.result() == (200, "Steered: Only Monday, please")
        assert call(f"{agent}/{k}/status")[1]["answering"] is None


def test_steer_with_no_turn_is_a_message_of_its_own(agent):
    k = key()
    status, steered = call(f"{agent}/{k}/steer", "hello")
    assert status == 200 and not steered["steered"], steered
    attached = request("GET", f"{INGRESS}/restate/invocation/{steered['invocationId']}/attach")
    assert attached == (200, "You said: hello")


def test_restate_steps_of_a_graph():
    k = key()
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"refunds/{k}/chat", "103", None, 60)
        wait_until(lambda: len(pending("refunds", k)) == 1, timeout=20, what="the wait")
        [waiting] = pending("refunds", k)
        assert (waiting["name"], waiting["input"]) == ("approval", {"order": "103"})
        assert call(f"refunds/{k}/signal", {"name": "approval", "value": "approved by Kim"})[0] == 200
        assert answer.result() == (200, "order 103: delivered / approved by Kim / refunded 103")
    wait_until(lambda: call("notes/refunds-103/list") == (200, ["refunded 103"]), timeout=10, what="the note")
    # A handler's terminal error fails the turn.
    status, body = call(f"refunds/{key()}/chat", "999")
    assert status == 500 and "no order 999" in str(body), body


def test_a_desk_of_your_own_asks_an_agent_once():
    k = key()
    sent = call(f"desk/{k}/chat", "hello", {"idempotency-key": f"desk-{k}"})
    again = call(f"desk/{k}/chat", "hello", {"idempotency-key": f"desk-{k}"})
    assert sent == again == (200, "You said: hello")
    status, page = call(f"helper/{k}/messages")
    assert status == 200 and [m["parts"][0]["text"] for m in page["messages"]] == ["hello", "You said: hello"]


def test_compact_and_reset_take_their_input_as_text_or_nothing():
    k = key()
    assert call(f"notebook/{k}/chat", "hello") == (200, "You said: hello")
    status, summary = request("POST", f"{INGRESS}/notebook/{k}/compact", None)
    assert status == 200 and summary.startswith("You said: Summarize our conversation"), summary
    status, page = call(f"notebook/{k}/messages")
    assert [m["parts"][0]["text"] for m in page["messages"]][1] == "Understood: I will carry on from there."
    plain = urllib.request.Request(f"{INGRESS}/notebook/{k}/reset", b"Alice likes short answers", method="POST")
    with urllib.request.urlopen(plain, timeout=30) as response:
        assert response.status == 200
    status, page = call(f"notebook/{k}/messages")
    assert page["messages"][0]["parts"][0]["text"] == "Before this conversation: Alice likes short answers"
    assert request("POST", f"{INGRESS}/notebook/{k}/reset", None)[0] == 200
    assert call(f"notebook/{k}/messages") == (200, {"messages": [], "more": False})

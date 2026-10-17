"""
Restate's tools for agents (``call``, ``send``, ``workflow``, ``subagent`` and
``signal``), on the scripted model, against a real Restate server: the
support agent of services.py.

    make concurrency-test-py   # gets restate-server first
"""

import json
import uuid
from concurrent.futures import ThreadPoolExecutor

from harness import ADMIN, INGRESS, call, request, wait_until


def key() -> str:
    return uuid.uuid4().hex[:12]


def use(*steps: tuple[str, dict]) -> str:
    """A prompt for the scripted model: these tool calls in turn, then their results joined by " / "."""
    return "use " + " then ".join(f"{tool} {json.dumps(args)}" for tool, args in steps)


REFUND = {"order": "103", "amount": 5}
SUPPORT = use(("orders_status", {"order": "103"}), ("wait_for_approval", REFUND), ("refund", REFUND))


def pending(k: str) -> list[dict]:
    status, waits = call(f"support/{k}/pending")
    assert status == 200, waits
    return waits


def test_call_signal_and_workflow():
    k = key()
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"support/{k}/chat", SUPPORT, None, 60)
        wait_until(lambda: len(pending(k)) == 1, timeout=20, what="the wait")
        [waiting] = pending(k)
        assert (waiting["name"], waiting["input"]) == ("approval", REFUND)
        assert call(f"support/{k}/signal", {"id": waiting["id"], "value": "approved by Kim"}) == (
            200,
            {"resolved": waiting["id"]},
        )
        assert answer.result() == (200, "order 103: delivered / approved by Kim / refunded 103")
    assert pending(k) == []


def test_a_signal_no_one_waits_for_is_dropped_unless_kept():
    dropped, kept = key(), key()
    assert call(f"support/{dropped}/signal", {"name": "approval", "value": "early"}) == (200, {"resolved": None})
    assert call(f"support/{kept}/signal", {"name": "approval", "value": "kept", "keep": True}) == (
        200,
        {"resolved": None},
    )
    assert call(f"support/{kept}/chat", use(("wait_for_approval", REFUND))) == (200, "kept")
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"support/{dropped}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(dropped)) == 1, timeout=20, what="the wait")
        assert call(f"support/{dropped}/signal", {"name": "approval", "value": "fresh"})[0] == 200
        assert answer.result() == (200, "fresh")


def test_a_cancelled_wait_is_forgotten():
    k = key()
    status, body = request("POST", f"{INGRESS}/support/{k}/chat/send", use(("wait_for_approval", REFUND)))
    assert status == 202, body
    wait_until(lambda: len(pending(k)) == 1, timeout=20, what="the wait")
    status, _ = request("PATCH", f"{ADMIN}/invocations/{body['invocationId']}/cancel")
    assert status in (200, 202)
    wait_until(lambda: pending(k) == [], timeout=20, what="the wait to be forgotten")


def test_a_handlers_terminal_error_is_the_tools_answer():
    assert call(f"support/{key()}/chat", use(("orders_status", {"order": "999"}))) == (200, "no order 999")


def test_a_subagent_continues_its_task_by_name():
    k = key()
    task = use(("helper", {"name": "greet", "task": "hello"}))
    assert call(f"support/{k}/chat", task) == (200, "You said: hello")
    status, page = call(f"helper/{k}.greet/messages")
    assert status == 200 and [m["parts"][0]["text"] for m in page["messages"]] == ["hello", "You said: hello"]


def test_send_does_not_wait():
    k, other = key(), key()
    sent = use(("notes_add", {"key": other, "note": "call back"}))
    assert call(f"support/{k}/chat", sent) == (200, f"sent to notes/{other}/add")
    wait_until(lambda: call(f"notes/{other}/list") == (200, ["call back"]), timeout=10, what="the note")


def test_status_and_stop_a_turn():
    k = key()
    assert call(f"support/{k}/status") == (200, {"answering": None, "waiting": []})
    assert call(f"support/{k}/stop") == (200, {"stopped": None})
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"support/{k}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(k)) == 1, timeout=20, what="the wait")
        status, now = call(f"support/{k}/status")
        assert status == 200 and now["answering"]["invocationId"].startswith("inv_") and len(now["waiting"]) == 1
        assert call(f"support/{k}/stop") == (200, {"stopped": now["answering"]["invocationId"]})
        status, body = answer.result()
        assert status == 409, body
    wait_until(lambda: call(f"support/{k}/status") == (200, {"answering": None, "waiting": []}), timeout=20)
    # The next message starts afresh.
    assert call(f"support/{k}/chat", "hello") == (200, "You said: hello")


def test_steer_a_turn_at_its_next_tool_call():
    k = key()
    with ThreadPoolExecutor() as pool:
        answer = pool.submit(call, f"support/{k}/chat", use(("wait_for_approval", REFUND)), None, 60)
        wait_until(lambda: len(pending(k)) == 1, timeout=20, what="the wait")
        status, steered = call(f"support/{k}/steer", "Only Monday, please")
        assert status == 200 and steered["steered"], steered
        assert call(f"support/{k}/signal", {"name": "approval", "value": "approved"})[0] == 200
        assert answer.result() == (200, "Steered: Only Monday, please")
        assert call(f"support/{k}/status")[1]["answering"] is None


def test_steer_with_no_turn_is_a_message_of_its_own():
    k = key()
    status, steered = call(f"support/{k}/steer", "hello")
    assert status == 200 and not steered["steered"], steered
    attached = request("GET", f"{INGRESS}/restate/invocation/{steered['invocationId']}/attach")
    assert attached == (200, "You said: hello")

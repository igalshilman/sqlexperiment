"""
The Python SDK without journal replay, and SQL in its objects, against a real
Restate server: the examples, retries, suspensions, concurrency, cancellation.

    make concurrency-test-py   # gets restate-server first
"""

import time
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest
from harness import ADMIN, INGRESS, Server, Service, call, request, wait_until

HERE = Path(__file__).resolve().parent


@pytest.fixture(scope="module", autouse=True)
def restate_server(tmp_path_factory) -> Iterator[None]:
    logs = tmp_path_factory.mktemp("logs")
    server = Server(logs / "server.log")
    service = Service([HERE / "services.py"], 9181, logs / "services.log")
    server.start()
    service.start()
    service.register()
    try:
        yield
    finally:
        service.stop()
        server.kill()


def key() -> str:
    """An object of its own, for each test."""
    return uuid.uuid4().hex[:12]


def test_notes():
    k = key()
    assert call(f"notes/{k}/add", "Buy milk") == (200, None)
    assert call(f"notes/{k}/add", "Walk the dog") == (200, None)
    assert call(f"notes/{k}/list") == (200, ["Buy milk", "Walk the dog"])


def test_bank_moves_money_once():
    alice, bob = key(), key()
    assert call(f"account/{alice}/deposit", {"id": "d1", "amount": 100}) == (200, 100)
    assert call(f"account/{alice}/deposit", {"id": "d1", "amount": 100}) == (200, 100)
    assert call(f"account/{alice}/transfer", {"id": "t1", "to": bob, "amount": 30}) == (200, 70)
    assert call(f"account/{alice}/transfer", {"id": "t1", "to": bob, "amount": 30}) == (200, 70)
    status, body = call(f"account/{alice}/transfer", {"id": "t2", "to": bob, "amount": 300})
    assert status == 500 and "insufficient funds" in body["message"]
    # The credit is a send, made once the debit commits.
    wait_until(lambda: call(f"account/{bob}/balance") == (200, 30), timeout=10, what="the credit")
    assert call(f"account/{bob}/history") == (200, [{"id": "t1", "amount": 30}])
    assert call(f"account/{alice}/history") == (200, [{"id": "d1", "amount": 100}, {"id": "t1", "amount": -30}])


def test_an_sql_error_is_terminal():
    status, body = call(f"probe/{key()}/sql_error")
    assert status == 500
    assert "no such table: missing" in body["message"]


def test_a_retried_attempt_runs_again_and_sees_the_commits_before_it():
    assert call(f"probe/{key()}/flaky") == (200, 3)


def test_a_suspended_handler_runs_again_and_attaches_to_its_call():
    started = time.monotonic()
    status, body = call(f"probe/{key()}/waits")
    assert status == 200
    # It suspended (inactivity timeout 1s) while the 3s call ran, then ran again.
    assert body.startswith("done after ") and body != "done after 1 runs", body
    assert time.monotonic() - started < 15


def test_calls_awaited_together_run_at_once():
    words = ["a", "b", "c", "d", "e"]
    started = time.monotonic()
    assert call(f"probe/{key()}/fan_out", words) == (200, [w.upper() for w in words])
    # Each takes 0.2s; one after the other they would take 1s.
    assert time.monotonic() - started < 0.8


def test_a_call_starts_when_it_is_made():
    started = time.monotonic()
    assert call(f"probe/{key()}/started") == (200, "EARLY")
    # The 0.2s call ran during the 0.5s sleep.
    assert time.monotonic() - started < 0.65


def test_state_and_sleeps_combine_with_asyncio():
    k = key()
    assert call(f"probe/{k}/combined") == (200, [None, 1, 1])
    assert call(f"probe/{k}/combined") == (200, [1, 1, 1])


def test_run_and_awakeables_are_refused():
    status, body = call(f"probe/{key()}/run_refused")
    assert status == 500 and "ctx.run is not available without journal replay" in body["message"]
    status, body = call(f"probe/{key()}/awakeable_refused")
    assert status == 500 and "awakeables are not available without journal replay" in body["message"]


def test_a_cancellation_reaches_every_pending_wait():
    k = key()
    status, body = request("POST", f"{INGRESS}/probe/{k}/cancellable/send")
    assert status == 202, body
    invocation = body["invocationId"]
    wait_until(
        lambda: request("PATCH", f"{ADMIN}/invocations/{invocation}/cancel")[0] in (200, 202),
        timeout=10,
        what="the cancellation",
    )
    wait_until(lambda: call(f"probe/{k}/cancellations") == (200, 1), timeout=10, what="the handler to see it")

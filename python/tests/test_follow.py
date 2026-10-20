"""
Following an agent's conversation with ``restate_agents.client``, against a
real Restate server: ``notifications``, ``watch`` and ``follow``, on the
support agents of services.py (LangGraph, Pydantic AI and Claude), through a
turn that waits for an approval.

    make concurrency-test-py   # gets restate-server first
"""

import asyncio
import json
import time
import uuid

import httpx
import pytest
from harness import INGRESS
from restate_agents.client import AgentClient, AgentError

REFUND = {"order": "103", "amount": 5}
SUPPORT = "use " + " then ".join(
    f"{tool} {json.dumps(args)}"
    for tool, args in (("orders_status", {"order": "103"}), ("wait_for_approval", REFUND), ("refund", REFUND))
)


@pytest.fixture(params=["support", "pydantic_support", "claude_support"])
def agent(request) -> str:
    return request.param


def client(agent: str) -> AgentClient:
    return AgentClient(INGRESS, agent, uuid.uuid4().hex[:12])


async def test_a_watch_answers_at_its_timeout_with_the_revision(agent):
    async with client(agent) as alice:
        revision = await alice.notifications()
        started = time.monotonic()
        assert await alice.watch(revision, timeout_seconds=1) == revision
        assert 0.9 < time.monotonic() - started < 10


async def test_follow_sees_a_turn_from_its_start_to_its_answer(agent):
    async with client(agent) as alice:
        sent = await alice.send(SUPPORT)
        messages: dict[int, dict] = {}
        approved = False
        async with asyncio.timeout(60):
            async for change in alice.follow(timeout_seconds=5):
                messages.update({m["id"]: m for m in change.messages})
                waiting = change.status["waiting"]
                if waiting and not approved:
                    [wait] = waiting
                    assert (wait["name"], wait["input"]) == ("approval", REFUND)
                    assert await alice.signal("approved by Kim", id=wait["id"]) == wait["id"]
                    approved = True
                answer = "order 103: delivered / approved by Kim / refunded 103"
                texts = [p.get("text") for m in messages.values() for p in m["parts"]]
                if change.status["answering"] is None and answer in texts:
                    break
        assert approved
        assert await alice.answer(sent) == answer
        # The tool calls, each with its output: the approval's too.
        calls = {p["name"]: p.get("output") for m in messages.values() for p in m["parts"] if p["type"] == "tool"}
        assert calls["wait_for_approval"] == "approved by Kim"


async def test_a_changed_revision_ends_a_watch_at_once(agent):
    async with client(agent) as alice:
        revision = await alice.notifications()
        watching = asyncio.create_task(alice.watch(revision, timeout_seconds=50))
        await asyncio.sleep(0.5)
        started = time.monotonic()
        assert await alice.chat("hello") == "You said: hello"
        assert await asyncio.wait_for(watching, 10) > revision
        assert time.monotonic() - started < 10


async def test_an_error_says_what_failed():
    async with AgentClient(INGRESS, "no_such_agent", "alice") as missing:
        with pytest.raises(AgentError) as error:
            await missing.status()
        assert error.value.status == 404


@pytest.mark.parametrize("handler", ["status", "notifications", "pending", "stop"])
async def test_handlers_without_input_take_a_request_as_restates_typescript_client_sends_it(handler):
    """An empty body, with JSON's content type: the chat page of examples-typescript/ui sends that."""
    async with httpx.AsyncClient() as http:
        response = await http.post(
            f"{INGRESS}/support/{uuid.uuid4().hex[:12]}/{handler}",
            content=b"",
            headers={"content-type": "application/json"},
        )
    assert response.status_code == 200, response.text

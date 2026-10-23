"""Tools that run in a sandbox (restate_agents.sandbox), on the local provider and a fake context."""

import asyncio
import json

import pytest
import restate_fns as restate
from fakes import ClaudeClient, Object, Teller
from restate_agents import claude, sandbox
from restate_agents.object import _service


def tools(box):
    return {t.name: t for t in box.tools()}


def handler(box, *calls):
    """A handler of the agent ``analyst`` that makes the tool calls ``calls`` (name, input), and answers with theirs."""

    async def run(ctx):
        _service.set("analyst")
        by_name = tools(box)
        return [await by_name[name].invoke(input, f"call-{n}") for n, (name, input) in enumerate(calls)]

    return run


async def test_a_conversations_sandbox_keeps_its_files_from_one_message_to_the_next(tmp_path):
    box = sandbox.local(tmp_path)
    alice = Object("alice")
    await alice.invoke(handler(box, ("write_file", {"path": "data/notes.txt", "content": "Lisbon"})))
    [listed, read] = await alice.invoke(
        handler(box, ("list_files", {"path": "data"}), ("read_file", {"path": "data/notes.txt"}))
    )
    assert (listed, read) == (["notes.txt"], "Lisbon")
    # Another conversation has a sandbox of its own.
    [listed] = await Object("bob").invoke(handler(box, ("list_files", {})))
    assert listed == []


async def test_a_command_runs_in_the_workspace_without_the_processs_environment(tmp_path, monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-secret")
    box = sandbox.local(tmp_path)
    [_, ran] = await Object().invoke(
        handler(
            box,
            (
                "write_file",
                {"path": "hello.py", "content": "import os; print('hi', os.environ.get('ANTHROPIC_API_KEY'))"},
            ),
            ("run_command", {"command": "python3 hello.py && exit 3"}),
        )
    )
    assert json.loads(ran) == {"exit_code": 3, "stdout": "hi None\n", "stderr": ""}


async def test_a_command_past_its_timeout_is_stopped(tmp_path):
    [ran] = await Object().invoke(
        handler(sandbox.local(tmp_path), ("run_command", {"command": "sleep 5", "timeout_seconds": 1}))
    )
    assert json.loads(ran)["exit_code"] is None


async def test_a_missing_file_or_one_outside_the_workspace_is_the_tools_answer(tmp_path):
    box = sandbox.local(tmp_path)
    for path in ("missing.txt", "../../etc/passwd"):
        with pytest.raises(restate.TerminalError):
            await Object().invoke(handler(box, ("read_file", {"path": path})))


class Counting:
    """The local provider, counting how often a sandbox is opened."""

    def __init__(self, root):
        self.local = sandbox.Local(root)
        self.opened = 0

    async def open(self, name, ref):
        self.opened += 1
        await asyncio.sleep(0.01)  # as a remote provider takes a while
        return await self.local.open(name, ref)

    async def destroy(self, ref):
        await self.local.destroy(ref)


async def test_parallel_tool_calls_open_the_sandbox_once(tmp_path):
    provider = Counting(tmp_path)
    box = sandbox.Sandbox(provider)

    async def parallel(ctx):
        _service.set("analyst")
        by_name = tools(box)
        writes = [by_name["write_file"].invoke({"path": f"{n}.txt", "content": "x"}, f"c{n}") for n in range(3)]
        await asyncio.gather(*writes)
        return await by_name["list_files"].invoke({}, "c3")

    assert await Object().invoke(parallel) == ["0.txt", "1.txt", "2.txt"]
    assert provider.opened == 1


async def test_destroy_removes_the_sandbox_and_its_files(tmp_path):
    box = sandbox.local(tmp_path)
    alice = Object("alice")
    await alice.invoke(handler(box, ("write_file", {"path": "a.txt", "content": "x"})))

    async def destroy(ctx):
        _service.set("analyst")
        await box.destroy(ctx)

    await alice.invoke(destroy)
    assert list(tmp_path.iterdir()) == []


async def test_an_agent_writes_and_runs_code_in_its_sandbox_and_finds_it_in_the_next_message(tmp_path):
    analyst = claude.agent("analyst", client=ClaudeClient(Teller()), tools=sandbox.local(tmp_path).tools())
    chat = analyst.handlers["chat"].fn
    alice = Object("alice")
    script = json.dumps({"path": "answer.py", "content": "print(6 * 7)"})
    answer = await alice.invoke(chat, f'use write_file {script} then run_command {{"command": "python3 answer.py"}}')
    assert answer.startswith("Wrote 12 bytes to answer.py / ") and '"stdout": "42\\n"' in answer
    assert await alice.invoke(chat, 'use read_file {"path": "answer.py"}') == "print(6 * 7)"

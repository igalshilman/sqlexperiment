"""
Codex agents in Restate virtual objects: OpenAI's coding agent (its loop,
its tools in a workspace, its compaction), each conversation a Codex thread
whose rollout (its transcript) is stored in the object's database::

    from restate_agents import codex, serve

    coder = codex.agent("coder", workspace="/work/coder", system="Keep the tests green.")
    serve(coder)

The model is ``model`` (``openai/model-id``), or else MODEL if it is
OpenAI's, or else Codex's default, with OPENAI_API_KEY. Each prompt runs
``codex app-server`` (of ``openai-codex-cli-bin``, the binary of OpenAI's
Codex SDK) in a process of its own, speaking its JSON-RPC protocol over
stdio; the tools here are its dynamic tools, which it calls back here.

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and resumes the Codex thread from its stored
rollout, synced before each tool call of the tools here and at each turn's
end:

- A message is answered once per invocation: its answer is stored with its
  prompt, and a retried attempt returns it.
- A turn that a crash cut off is carried on: the retried attempt resumes the
  thread and asks Codex to finish the message. A tool call made again with
  the same input as one the crash cut off (or finished before its result
  was stored) has that call's id, so a tool that passes its ``call_id`` on
  (as Restate's tools do) makes the call once; one that finished answers
  with its stored result.
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A cancelled turn (``stop``) ends at its next tool call, answered
  "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) joins it after the next tool call,
  as Codex steers a turn (``turn/steer``).
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import tempfile
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from codex_cli_bin import bundled_codex_path
from restate_fns import ObjectContext

from restate_agents import harness, turns
from restate_agents.common import Transcript, configured, model_id_of, table_schema
from restate_agents.object import Object
from restate_agents.tools import Tool
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "agent", "prompt", "reset"]

# The tables of the conversations, of their tool calls, and of the threads' rollouts.
PROMPTS, CALLS, ROLLOUTS, THREADS = (
    "restate_codex_prompts",
    "restate_codex_calls",
    "restate_codex_rollouts",
    "restate_codex_threads",
)


def agent(
    name: str,
    *,
    model: str | Callable[[ObjectContext], str] | None = None,
    system: str | Callable[[ObjectContext], str] | None = None,
    tools: Sequence[Tool] | Callable[[ObjectContext], Sequence[Tool]] = (),
    workspace: str | Path | Callable[[ObjectContext], str | Path] | None = None,
    config: dict[str, Any] | Callable[[ObjectContext], dict[str, Any]] | None = None,
    **options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    ``system`` is added to Codex's own instructions (its developer
    instructions). ``tools``: ``tool``s and Restate's tools. ``workspace``:
    a directory of the local machine to work in, which Codex's own tools may
    write (its sandbox's ``workspace-write``); without one, Codex works in an
    empty directory, and its sandbox is ``read-only``. ``config``: Codex's
    configuration, as ``config.toml`` has it (``{"model_reasoning_effort":
    "high"}``, a model provider of your own). Each may be a function of the
    invocation's context, or an async one (a workspace for each key, say).
    ``options``: those of ``restate.VirtualObject``. Add handlers of your own
    with ``@agent.handler``.
    """
    if not callable(model):
        _model_of(model)

    async def answer(ctx: ObjectContext, message: str) -> str:
        return await prompt(
            ctx,
            message,
            model=await configured(model, ctx),
            system=await configured(system, ctx),
            tools=await configured(tools, ctx),
            workspace=await configured(workspace, ctx),
            config=await configured(config, ctx),
        )

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        for line in await _Rollout(ctx.db, ctx.key()).load():
            _transcribe(transcript, line)
        return transcript

    return turns.agent(name, answer, transcript, **options)


async def prompt(
    ctx: Any,
    text: str,
    *,
    model: str | None = None,
    system: str | None = None,
    tools: Sequence[Tool] = (),
    workspace: str | Path | None = None,
    config: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Answer ``text`` in the conversation ``thread`` (by default, the object's
    key), and return the answer. In a handler of your own::

        @coder.handler
        async def review(ctx, branch: str) -> str:
            return await codex.prompt(ctx, f"Review the changes on {branch}", workspace="/work/coder")

    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    thread = thread or ctx.key()
    run = _Run(ctx, thread, _model_of(model), system, list(tools), workspace, config or {})
    return await harness.answer_once(ctx, harness.Prompts(ctx.db, PROMPTS, thread), text, run.answer)


async def reset(ctx: Any, handoff: str | None = None, *, thread: str | None = None) -> None:
    """
    Start the conversation ``thread`` (by default, the object's key) over: a
    new Codex thread, which begins with ``handoff`` as what to carry on with,
    if given (such as "Alice likes short answers"). In a handler of your own::

        @coder.handler(message=True)
        async def reset(ctx, handoff: str) -> None:
            await codex.reset(ctx, handoff)
    """
    thread = thread or ctx.key()
    await harness.Prompts(ctx.db, PROMPTS, thread).idle()
    await _Rollout(ctx.db, thread).reset(handoff)
    await turns.publish()


def _model_of(model: str | None) -> str | None:
    """The model id: of ``model``, or of MODEL if it is OpenAI's; or None, for Codex's default."""
    if model is None:
        named = os.environ.get("MODEL", "")
        model = named if named.startswith("openai/") else None
    return model_id_of(model, "openai") if model is not None else None


class _Rollout:
    """A conversation's Codex thread in the object's database: its rollout, a line per row, and its handoff."""

    def __init__(self, db: Any, thread: str) -> None:
        self.db = db
        self.thread = thread
        self.length = 0

    async def _tables(self) -> None:
        await table_schema(
            ROLLOUTS,
            f"""
            CREATE TABLE {ROLLOUTS} (
              thread TEXT NOT NULL,
              n INTEGER NOT NULL,
              line TEXT NOT NULL,
              PRIMARY KEY (thread, n)
            )
            """,
        ).ensure(self.db)
        await table_schema(
            THREADS, f"CREATE TABLE {THREADS} (thread TEXT PRIMARY KEY, path TEXT, handoff TEXT)"
        ).ensure(self.db)

    async def load(self) -> list[str]:
        """The rollout's lines, as stored."""
        await self._tables()
        rows = await self.db.query(f"SELECT line FROM {ROLLOUTS} WHERE thread = ? ORDER BY n", [self.thread])
        self.length = len(rows)
        return [line for (line,) in rows]

    async def place(self) -> tuple[str | None, str | None]:
        """Where the rollout goes in Codex's home (once it was started), and the handoff its thread begins with."""
        await self._tables()
        rows = await self.db.query(f"SELECT path, handoff FROM {THREADS} WHERE thread = ?", [self.thread])
        return (rows[0][0], rows[0][1]) if rows else (None, None)

    async def started(self, path: str) -> None:
        """The thread was started: its rollout is at ``path`` in Codex's home (its handoff, once it has lines)."""
        await self.db.execute(
            f"INSERT INTO {THREADS} (thread, path) VALUES (?, ?) "
            "ON CONFLICT (thread) DO UPDATE SET path = excluded.path",
            [self.thread, path],
        )

    async def sync(self, file: Path) -> None:
        """Store the lines of the rollout ``file`` that are not stored yet: those it has whole."""
        if not file.exists():
            return
        lines = file.read_text().split("\n")[:-1]  # the last line is whole once a newline ends it
        if len(lines) <= self.length:
            return
        async with self.db.transaction() as tx:
            for n in range(self.length, len(lines)):
                await tx.execute(f"INSERT INTO {ROLLOUTS} VALUES (?, ?, ?)", [self.thread, n, lines[n]])
        self.length = len(lines)
        await turns.publish()

    async def reset(self, handoff: str | None) -> None:
        await self._tables()
        async with self.db.transaction() as tx:
            await tx.execute(f"DELETE FROM {ROLLOUTS} WHERE thread = ?", [self.thread])
            await tx.execute(f"INSERT OR REPLACE INTO {THREADS} VALUES (?, NULL, ?)", [self.thread, handoff or None])


class _AppServer:
    """``codex app-server``, in a process of its own: its JSON-RPC requests, notifications and server requests."""

    def __init__(self, home: str, serve: Callable[[str, dict[str, Any]], Any]) -> None:
        self.home = home
        self.serve = serve
        self.notifications: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._next = 0
        self._tasks: set[asyncio.Task[Any]] = set()
        self.process: asyncio.subprocess.Process | None = None

    async def start(self) -> None:
        self.process = await asyncio.create_subprocess_exec(
            str(bundled_codex_path()),
            "app-server",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
            env={**os.environ, "CODEX_HOME": self.home},
            limit=1 << 24,
        )
        self._reader = asyncio.ensure_future(self._read())
        await self.request(
            "initialize",
            {"clientInfo": {"name": "restate-agents", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}},
        )
        self._send({"method": "initialized"})

    async def close(self) -> None:
        if self.process is None or self.process.stdin is None or self.process.stdin.is_closing():
            return
        self.process.stdin.close()
        try:
            await asyncio.wait_for(self.process.wait(), 5)
        except TimeoutError:
            self.process.kill()
            await self.process.wait()
        self._reader.cancel()
        for task in self._tasks:
            task.cancel()

    async def request(self, method: str, params: dict[str, Any]) -> Any:
        self._next += 1
        answer = asyncio.get_running_loop().create_future()
        self._pending[self._next] = answer
        self._send({"id": self._next, "method": method, "params": params})
        return await answer

    def _send(self, message: dict[str, Any]) -> None:
        assert self.process is not None and self.process.stdin is not None
        self.process.stdin.write((json.dumps(message) + "\n").encode())

    async def _read(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        while line := await self.process.stdout.readline():
            message = json.loads(line)
            if "method" in message and "id" in message:
                task = asyncio.ensure_future(self._answer(message))
                self._tasks.add(task)
                task.add_done_callback(self._tasks.discard)
            elif "id" in message:
                answer = self._pending.pop(message["id"])
                if "error" in message:
                    answer.set_exception(RuntimeError(f"Codex: {message['error'].get('message', message['error'])}"))
                else:
                    answer.set_result(message.get("result"))
            else:
                await self.notifications.put(message)
        await self.notifications.put({"method": "exited"})

    async def _answer(self, request: dict[str, Any]) -> None:
        result = await self.serve(request["method"], request.get("params") or {})
        self._send({"id": request["id"], "result": result})


@dataclass
class _Run:
    """A prompt's run: Codex, on the conversation's thread (resumed from its rollout), with the tools here."""

    ctx: Any
    thread: str
    model: str | None
    system: str | None
    tools: list[Tool]
    workspace: str | Path | None
    config: dict[str, Any]
    # Why the attempt fails: a tool's error, a cancellation, or a failed store; it interrupts the turn.
    failure: BaseException | None = None

    async def answer(self, pending: harness.Prompt, message: str) -> str:
        """The answer to ``message``, of the prompt ``pending``."""
        rollout = _Rollout(self.ctx.db, self.thread)
        lines = await rollout.load()
        path, handoff = await rollout.place()
        if handoff and not lines:
            message = f"Before this conversation: {handoff}\n\n{message}"
        home = tempfile.mkdtemp(prefix="restate-agents-codex-")
        prompts = harness.Prompts(self.ctx.db, PROMPTS, self.thread)
        tools = _Tools(self, pending, harness.Calls(self.ctx.db, CALLS, pending.prompt), prompts, rollout)
        server = tools.server = _AppServer(home, tools.serve)
        try:
            await server.start()
            if key := os.environ.get("OPENAI_API_KEY"):
                await server.request("account/login/start", {"type": "apiKey", "apiKey": key})
            id, tools.file = await self._thread(server, rollout, home, path, lines)
            turn = await server.request("turn/start", {"threadId": id, "input": [{"type": "text", "text": message}]})
            answer = await self._until_completed(server, turn["turn"]["id"])
            await server.close()  # Codex has written the whole rollout once it exits
            await rollout.sync(tools.file)
        finally:
            await server.close()
            shutil.rmtree(home, ignore_errors=True)
        if self.failure is not None:
            raise self.failure
        return answer

    async def _thread(
        self, server: _AppServer, rollout: _Rollout, home: str, path: str | None, lines: list[str]
    ) -> tuple[str, Path]:
        """
        The conversation's Codex thread, and its rollout file in ``home``:
        resumed from the stored rollout, or else started, with its place stored.
        """
        settings = {
            "cwd": str(self.workspace) if self.workspace is not None else harness.nowhere(),
            "approvalPolicy": "never",
            "sandbox": "workspace-write" if self.workspace is not None else "read-only",
            "config": self.config or None,
            "model": self.model,
            "developerInstructions": self.system,
        }
        if path is not None and lines:
            file = Path(home) / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("".join(line + "\n" for line in lines))
            thread = json.loads(lines[0])["payload"]["id"]
            resumed = await server.request("thread/resume", {"threadId": thread, "path": str(file), **settings})
            return resumed["thread"]["id"], file
        started = await server.request("thread/start", {**settings, "dynamicTools": [_spec(t) for t in self.tools]})
        file = Path(started["thread"]["path"])
        await rollout.started(str(file.relative_to(home)))
        return started["thread"]["id"], file

    def fail(self, error: BaseException) -> None:
        self.failure = self.failure or error

    async def _until_completed(self, server: _AppServer, turn: str) -> str:
        """The turn's answer, once it completed: its last agent message."""
        texts: list[str] = []
        while True:
            note = await server.notifications.get()
            method, params = note["method"], note.get("params") or {}
            if method == "exited":
                raise RuntimeError("Codex exited before the turn completed")
            if method == "item/completed" and params["item"].get("type") == "agentMessage":
                texts.append(params["item"].get("text") or "")
            if method == "turn/completed" and params["turn"]["id"] == turn:
                done = params["turn"]
                if done.get("status") == "failed" and self.failure is None:
                    raise RuntimeError(f"Codex failed: {(done.get('error') or {}).get('message', 'the turn failed')}")
                return texts[-1] if texts else ""


@dataclass
class _Tools:
    """
    The tools here, as a prompt's Codex calls them (dynamic tools): each call
    is stored once, with the rollout before it, and the messages steered to
    the turn follow its result.
    """

    run: _Run
    pending: harness.Prompt
    calls: harness.Calls
    prompts: harness.Prompts
    rollout: _Rollout
    # Codex, which calls them; and its rollout file, once the thread started.
    server: _AppServer = field(init=False)
    file: Path | None = None

    async def serve(self, method: str, params: dict[str, Any]) -> Any:
        """Answer Codex's request ``method``: a tool call; an approval is declined (the tools here need none)."""
        if method != "item/tool/call":
            return {"decision": "decline"} if method.endswith("requestApproval") else {}
        tool = next((t for t in self.run.tools if t.name == params["tool"]), None)
        if tool is None:
            return _content(f"Error: no tool {params['tool']}", success=False)
        if self.run.failure is not None:
            return _content("Error: the turn is ending", success=False)
        output, error, steered = await self._call(tool, params)
        self._follow_up(params, steered)
        return _content(output, success=not error)

    async def _call(self, tool: Tool, params: dict[str, Any]) -> tuple[str, bool, list[str]]:
        """The call's output, whether it is an error, and the messages steered to the turn; a failure fails the run."""
        try:
            if self.file is not None:
                await self.rollout.sync(self.file)  # the model's call, before the tool runs
            output, error = await self.calls.run(tool, params["callId"], params.get("arguments") or {}, self.run.fail)
            return output, error, await harness.steer(self.run.ctx, self.prompts, self.pending)
        except BaseException as failure:
            self.run.fail(failure)
            return "Error: the turn is ending", True, []

    def _follow_up(self, params: dict[str, Any], steered: list[str]) -> None:
        """
        A failed run's turn is interrupted, or the steered messages join it:
        requests of their own, which Codex answers once it has the tool's result.
        """
        thread, turn = params["threadId"], params["turnId"]
        if self.run.failure is not None:
            asyncio.ensure_future(self.server.request("turn/interrupt", {"threadId": thread, "turnId": turn}))
        elif steered:
            input = [{"type": "text", "text": text} for text in steered]
            steer = {"threadId": thread, "expectedTurnId": turn, "input": input}
            asyncio.ensure_future(self.server.request("turn/steer", steer))


def _content(text: str, *, success: bool) -> dict[str, Any]:
    """A dynamic tool's result."""
    return {"contentItems": [{"type": "inputText", "text": text}], "success": success}


def _spec(tool: Tool) -> dict[str, Any]:
    """``tool`` as a dynamic tool of Codex."""
    schema = tool.input.model_json_schema()
    return {"type": "function", "name": tool.name, "description": tool.description, "inputSchema": schema}


def _transcribe(transcript: Transcript, line: str) -> None:
    """A rollout line, as ``messages`` gives it: the user's and the assistant's messages, and the tool calls."""
    entry = json.loads(line)
    item = entry.get("payload") or {}
    if entry.get("type") != "response_item":
        return
    if item.get("type") == "message" and item.get("role") in ("user", "assistant"):
        text = "".join(c.get("text", "") for c in item.get("content") or [] if "text" in c)
        if item["role"] == "user" and text.startswith("<"):
            return  # Codex's context for the model, such as <environment_context>
        transcript.say(item["role"], text)
    elif item.get("type") == "function_call":
        try:
            input = json.loads(item.get("arguments") or "{}")
        except ValueError:
            input = item.get("arguments")
        transcript.call(item["call_id"], item["name"], input)
    elif item.get("type") == "function_call_output":
        output = item.get("output")
        if not isinstance(output, str):
            output = json.dumps(output)
        transcript.result(item["call_id"], output)

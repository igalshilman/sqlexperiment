"""
Claude Code agents in Restate virtual objects, on the Claude Agent SDK: the
agent loop of Claude Code itself (its tools, its compaction, its skills),
its session's transcript stored in the object's database::

    from restate_agents import claude_agent, serve

    coder = claude_agent.agent("coder", workspace="/work/coder", system="Keep the tests green.")
    serve(coder)

The model is ``model`` (``anthropic/model-id``), or else MODEL, or else
anthropic/claude-sonnet-5-5, with ANTHROPIC_API_KEY. Each prompt runs the
Claude Code CLI (the SDK's own, bundled) in a process of its own, with the
tools here served to it in this process (an SDK MCP server, ``restate``).

The handler is not replayed: a retried attempt (a crash, a lost connection)
runs again from the start, and Claude Code resumes its session from the
stored transcript (the SDK's ``SessionStore``, mirrored as the CLI writes):

- A message is answered once per invocation: its answer is stored with its
  prompt, and a retried attempt returns it.
- A turn that a crash cut off is carried on: the retried attempt resumes the
  session and asks Claude Code to finish the message, without repeating the
  tool calls that finished. A tool call made again with the same input as
  one the crash cut off (or finished before its result was stored) has that
  call's id, so a tool that passes its ``call_id`` on (as Restate's tools do)
  makes the call once; one that finished answers with its stored result.
- A tool's error fails the attempt, which Restate retries; but a
  ``restate.TerminalError``, which a retry would repeat, is the tool's
  answer to the model.
- A cancelled turn (``stop``) ends at its next tool call, answered
  "Cancelled", and the next message starts afresh.
- A message steered to a turn (``steer``) reaches the model after the next
  tool call, with its result (a ``PostToolUse`` hook's context).
"""

from __future__ import annotations

import inspect
import json
import shutil
import tempfile
import uuid
from collections import defaultdict, deque
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import restate_fns as restate
from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    HookMatcher,
    ResultMessage,
    SdkMcpTool,
    create_sdk_mcp_server,
    query,
)
from restate_fns import ObjectContext

from restate_agents import harness, turns
from restate_agents.common import DEFAULT_MODEL, Transcript, has_table, model_id_of, model_of
from restate_agents.object import Object, service
from restate_agents.tools import Tool
from restate_agents.turns import CANCELLED

__all__ = ["CANCELLED", "DEFAULT_MODEL", "agent", "prompt", "reset"]

# The MCP server that serves the agent's tools to Claude Code: a tool ``t`` is ``mcp__restate__t`` there.
SERVER = "restate"

# The tables of the conversations, of their tool calls, and of Claude Code's sessions.
PROMPTS, CALLS = "claude_agent_prompts", "claude_agent_calls"

# Claude Code's own tools that an agent with a workspace may use without asking.
WORKSPACE_TOOLS = ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]


def agent(
    name: str,
    *,
    model: str | Callable[[ObjectContext], str] | None = None,
    system: str | Callable[[ObjectContext], str] | None = None,
    tools: Sequence[Tool] | Callable[[ObjectContext], Sequence[Tool]] = (),
    workspace: str | Path | Callable[[ObjectContext], str | Path] | None = None,
    options: dict[str, Any] | Callable[[ObjectContext], dict[str, Any]] | None = None,
    **object_options: Any,
) -> Object:
    """
    An agent: a virtual object ``name``, one per key (its conversation), with the handlers

    - ``chat``: a message in, the answer out; the message as plain text
      (``curl -d 'Hi'``) or as a JSON string (``curl --json '"Hi"'``);
    - ``messages``: the conversation so far, a page at a time (``MessagesRequest``);
    - ``status``, ``stop`` and ``steer``: what it is doing, and the message it
      answers; ``pending`` and ``signal``: what its tools wait for.

    ``system`` is added to Claude Code's own system prompt. ``tools``: ``tool``s
    and Restate's tools. ``workspace``: a directory of the local machine to
    work in, with Claude Code's own tools (read, write, edit, bash, ...; use
    a sandbox); without one, the agent has only ``tools``. ``options``: more
    of ``ClaudeAgentOptions``, such as ``{"max_turns": 20}`` or
    ``{"setting_sources": ["project"]}``. Each may be a function of the
    invocation's context, or an async one (a workspace for each key, say).
    ``object_options``: those of ``restate.VirtualObject``. Add handlers of
    your own with ``@agent.handler``.
    """
    if not callable(model):
        model_id_of(model_of(model), "anthropic")

    async def answer(ctx: ObjectContext, message: str) -> str:
        async def given(option: Any) -> Any:
            value = option(ctx) if callable(option) else option
            return await value if inspect.isawaitable(value) else value

        return await prompt(
            ctx,
            message,
            model=await given(model),
            system=await given(system),
            tools=await given(tools),
            workspace=await given(workspace),
            options=await given(options),
        )

    async def transcript(ctx: ObjectContext) -> Transcript:
        transcript = Transcript()
        if not await has_table(ctx.db, _Store.TABLE):
            return transcript
        session = await _Threads(ctx.db).session(ctx.key())
        for entry in await _Store(ctx.db).load({"project_key": "", "session_id": session}) or []:
            _transcribe(transcript, entry)
        return transcript

    return turns.agent(name, answer, transcript, **object_options)


async def prompt(
    ctx: Any,
    text: str,
    *,
    model: str | None = None,
    system: str | None = None,
    tools: Sequence[Tool] = (),
    workspace: str | Path | None = None,
    options: dict[str, Any] | None = None,
    thread: str | None = None,
) -> str:
    """
    Answer ``text`` in the conversation ``thread`` (by default, the object's
    key), and return the answer. In a handler of your own::

        @coder.handler
        async def review(ctx, branch: str) -> str:
            return await claude_agent.prompt(ctx, f"Review the changes on {branch}", workspace="/work/coder")

    Each prompt is answered once, however often an attempt crashes: the
    invocation's first prompt has its id, the next ``<id>:2``, and so on.
    """
    thread = thread or ctx.key()
    run = _Run(
        ctx,
        thread,
        model=model_id_of(model_of(model), "anthropic"),
        system=system,
        tools=list(tools),
        workspace=workspace,
        options=options or {},
    )
    return await harness.answer_once(ctx, harness.Prompts(ctx.db, PROMPTS, thread), text, run.answer)


async def reset(ctx: Any, handoff: str | None = None, *, thread: str | None = None) -> None:
    """
    Start the conversation ``thread`` (by default, the object's key) over: a
    new Claude Code session, which begins with ``handoff`` as what to carry
    on with, if given (such as "Alice likes short answers"). In a handler of
    your own::

        @coder.handler(message=True)
        async def reset(ctx, handoff: str) -> None:
            await claude_agent.reset(ctx, handoff)
    """
    thread = thread or ctx.key()
    await harness.Prompts(ctx.db, PROMPTS, thread).idle()
    id = turns.prompt_id(ctx)
    await _Threads(ctx.db).start(thread, _session_of(f"{service()}/{thread}/{id}"), handoff)
    await turns.publish()


class _Threads:
    """Each conversation's Claude Code session, and the handoff its next message begins with, if any."""

    TABLE = "claude_agent_threads"

    def __init__(self, db: Any) -> None:
        self.db = db

    async def _table(self) -> None:
        if not await has_table(self.db, self.TABLE):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {self.TABLE} (thread TEXT PRIMARY KEY, session TEXT NOT NULL, handoff TEXT)
            """)

    async def get(self, thread: str) -> tuple[str, str | None]:
        """The session of ``thread`` (its first, unless it was reset), and its handoff."""
        if await has_table(self.db, self.TABLE):
            rows = await self.db.query(f"SELECT session, handoff FROM {self.TABLE} WHERE thread = ?", [thread])
            if rows:
                return rows[0][0], rows[0][1]
        return _session_of(f"{service()}/{thread}"), None

    async def session(self, thread: str) -> str:
        return (await self.get(thread))[0]

    async def start(self, thread: str, session: str, handoff: str | None) -> None:
        await self._table()
        await self.db.execute(
            f"INSERT OR REPLACE INTO {self.TABLE} VALUES (?, ?, ?)", [thread, session, handoff or None]
        )


def _session_of(name: str) -> str:
    """The Claude Code session of ``name``: a UUID, the same in each attempt."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"restate-agents:{name}"))


class _Store:
    """
    The SDK's ``SessionStore``, in the object's database: each session's
    transcript, an entry per row, in order. Sessions are UUIDs, so their
    project (Claude Code's working directory) does not key them.
    """

    TABLE = "claude_agent_entries"

    def __init__(self, db: Any, failed: Callable[[BaseException], None] = lambda error: None) -> None:
        self.db = db
        self.failed = failed

    async def _table(self) -> None:
        if not await has_table(self.db, self.TABLE):
            await self.db.execute(f"""
                CREATE TABLE IF NOT EXISTS {self.TABLE} (
                  session TEXT NOT NULL,
                  subpath TEXT NOT NULL,
                  n INTEGER NOT NULL,
                  uuid TEXT,
                  entry TEXT NOT NULL,
                  PRIMARY KEY (session, subpath, n),
                  UNIQUE (session, subpath, uuid)
                )
            """)

    async def append(self, key: dict[str, Any], entries: list[dict[str, Any]]) -> None:
        # The SDK logs an append's error and goes on: the attempt must fail instead.
        try:
            await self._table()
            session, subpath = key["session_id"], key.get("subpath") or ""
            async with self.db.transaction() as tx:
                [(n,)] = await tx.query(
                    f"SELECT COALESCE(MAX(n) + 1, 0) FROM {self.TABLE} WHERE session = ? AND subpath = ?",
                    [session, subpath],
                )
                for entry in entries:
                    await tx.execute(
                        f"INSERT OR IGNORE INTO {self.TABLE} VALUES (?, ?, ?, ?, ?)",
                        [session, subpath, n, entry.get("uuid"), json.dumps(entry)],
                    )
                    n += 1
            if not subpath:
                await turns.publish()
        except BaseException as error:
            self.failed(error)
            raise

    async def load(self, key: dict[str, Any]) -> list[dict[str, Any]] | None:
        await self._table()
        rows = await self.db.query(
            f"SELECT entry FROM {self.TABLE} WHERE session = ? AND subpath = ? ORDER BY n",
            [key["session_id"], key.get("subpath") or ""],
        )
        return [json.loads(entry) for (entry,) in rows] or None

    async def list_subkeys(self, key: dict[str, Any]) -> list[str]:
        await self._table()
        rows = await self.db.query(
            f"SELECT DISTINCT subpath FROM {self.TABLE} WHERE session = ? AND subpath != ''", [key["session_id"]]
        )
        return [subpath for (subpath,) in rows]


@dataclass
class _Run:
    """A prompt's run: Claude Code, resumed on the conversation's session, with the tools here."""

    ctx: Any
    thread: str
    model: str
    system: str | None
    tools: list[Tool]
    workspace: str | Path | None
    options: dict[str, Any]
    # Why the attempt fails: a tool's error, a cancellation, or a failed store; the run ends at its next message.
    failure: BaseException | None = None

    def fail(self, error: BaseException) -> None:
        self.failure = self.failure or error

    async def answer(self, pending: harness.Prompt, message: str) -> str:
        """The answer to ``message``, of the prompt ``pending``."""
        threads = _Threads(self.ctx.db)
        session, handoff = await threads.get(self.thread)
        store = _Store(self.ctx.db, self.fail)
        resume = await store.load({"project_key": "", "session_id": session}) is not None
        if handoff and not resume:
            message = f"Before this conversation: {handoff}\n\n{message}"
        ids: dict[tuple[str, str], deque[str]] = defaultdict(deque)
        calls = harness.Calls(self.ctx.db, CALLS, pending.prompt)
        prompts = harness.Prompts(self.ctx.db, PROMPTS, self.thread)

        async def before(input: dict[str, Any], tool_use_id: str | None, _: Any) -> dict[str, Any]:
            if tool_use_id:
                ids[(input["tool_name"], harness.canonical(input["tool_input"]))].append(tool_use_id)
            return {}

        async def after(input: dict[str, Any], tool_use_id: str | None, _: Any) -> dict[str, Any]:
            try:
                steered = await harness.steer(self.ctx, prompts, pending)
            except BaseException as error:
                self.fail(error)
                return {}
            if not steered:
                return {}
            context = "\n\n".join(steered)
            return {"hookSpecificOutput": {"hookEventName": input["hook_event_name"], "additionalContext": context}}

        def served(tool: Tool) -> SdkMcpTool[Any]:
            async def handler(args: dict[str, Any]) -> dict[str, Any]:
                if self.failure is not None:
                    return _result("Error: the turn is ending", error=True)
                waiting = ids[(f"mcp__{SERVER}__{tool.name}", harness.canonical(args))]
                id = waiting.popleft() if waiting else uuid.uuid4().hex
                output, error = await calls.run(tool, id, args, self.fail)
                return _result(output, error)

            return SdkMcpTool(tool.name, tool.description, tool.input.model_json_schema(), handler)

        # Claude Code's configuration, with its local copy of the transcript: the run's own, as the stored one counts.
        config = tempfile.mkdtemp(prefix="restate-agents-claude-")
        options = ClaudeAgentOptions(
            model=self.model,
            cwd=str(self.workspace) if self.workspace is not None else harness.nowhere(),
            system_prompt={"type": "preset", "preset": "claude_code", "append": self.system}
            if self.system
            else {"type": "preset", "preset": "claude_code"},
            mcp_servers={SERVER: create_sdk_mcp_server(SERVER, tools=[served(t) for t in self.tools])},
            allowed_tools=[f"mcp__{SERVER}__{t.name}" for t in self.tools]
            + (WORKSPACE_TOOLS if self.workspace is not None else []),
            hooks={"PreToolUse": [HookMatcher(hooks=[before])], "PostToolUse": [HookMatcher(hooks=[after])]},
            env={"CLAUDE_CONFIG_DIR": config, **self.options.get("env", {})},
            session_store=store,
            session_store_flush="eager",
            **({"resume": session} if resume else {"session_id": session}),
            **({} if self.workspace is not None else {"tools": []}),
            **{k: v for k, v in self.options.items() if k != "env"},
        )
        answer: str | None = None
        texts: list[str] = []
        messages = query(prompt=message, options=options)
        try:
            async for reply in messages:
                if self.failure is not None:
                    break
                if isinstance(reply, AssistantMessage):
                    texts = [block.text for block in reply.content if hasattr(block, "text")] or texts
                elif isinstance(reply, ResultMessage):
                    if reply.is_error:
                        failed = f"Claude Code failed: {reply.result or reply.subtype}"
                        # A limit of the options (max_turns, max_budget_usd), which a retry would reach again.
                        raise (
                            restate.TerminalError(failed)
                            if reply.subtype.startswith("error_max")
                            else RuntimeError(failed)
                        )
                    answer = reply.result if reply.result is not None else "".join(texts)
        finally:
            await messages.aclose()
            shutil.rmtree(config, ignore_errors=True)
        if self.failure is not None:
            raise self.failure
        if answer is None:
            raise RuntimeError("Claude Code ended without an answer")
        if handoff:
            await threads.start(self.thread, session, None)  # the new session has its handoff now
        return answer


def _result(text: str, error: bool = False) -> dict[str, Any]:
    """An MCP tool's result."""
    return {"content": [{"type": "text", "text": text}], **({"is_error": True} if error else {})}


def _transcribe(transcript: Transcript, entry: dict[str, Any]) -> None:
    """A transcript entry of Claude Code, as ``messages`` gives it: its users' and assistant's messages."""
    message = entry.get("message")
    if entry.get("type") not in ("user", "assistant") or entry.get("isMeta") or not isinstance(message, dict):
        return
    content = message.get("content")
    if isinstance(content, str):
        transcript.say(message["role"], content)
        return
    texts = "".join(b.get("text", "") for b in content if b.get("type") == "text")
    if texts or message["role"] == "assistant" and not any(b.get("type") == "tool_use" for b in content):
        transcript.say(message["role"], texts)
    for block in content:
        if block.get("type") == "tool_use":
            name = block["name"].removeprefix(f"mcp__{SERVER}__")
            transcript.call(block["id"], name, block.get("input"))
        elif block.get("type") == "tool_result":
            output = block.get("content")
            if isinstance(output, list):
                output = "".join(b.get("text", "") for b in output if b.get("type") == "text")
            transcript.result(block["tool_use_id"], output or "", bool(block.get("is_error")))

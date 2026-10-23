"""
Tools that run in a sandbox, while the agent's loop runs where it does: the
model lists, reads and writes files and runs commands in a workspace of the
conversation's own, which lasts from one message to the next::

    from restate_agents import claude, sandbox

    box = sandbox.modal(image="python:3.13-slim")
    analyst = claude.agent("analyst", tools=box.tools())

The first sandbox tool of an attempt opens the conversation's sandbox: it
makes it, or resumes the one it made before, whose reference is in the
object's database (``sandboxes``). Its name is the conversation's (the
object's service and key), so an attempt that retries after the sandbox
was made finds it rather than making another. Parallel tool calls share it.

A tool call is run again when a crash cut it off (each integration runs an
unfinished call again, with its id): a command that must not run twice
makes itself safe to, as with any tool. A missing file is the tool's answer
to the model; a command's exit code and output are its result, however it
ended.

Providers:

- ``modal(...)``: a Modal Sandbox, its files in a Volume of the conversation's
  (the files outlast the compute, which stops once idle and starts again on
  the next tool call); MODAL_TOKEN_ID and MODAL_TOKEN_SECRET.
- ``local(...)``: a directory of this machine, commands in it with a bare
  environment. For development: it isolates nothing.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import posixpath
import shutil
import tempfile
import weakref
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

import restate_fns as restate

from restate_agents.object import service
from restate_agents.tools import Tool, tool

TABLE = "sandboxes"
SCHEMA = restate.Schema("restate_agents.sandbox", [f"CREATE TABLE {TABLE} (name TEXT PRIMARY KEY, ref TEXT NOT NULL)"])
# What a tool gives the model of a file or a command's output, at most.
CLIPPED = 20_000


@dataclass(frozen=True)
class Ran:
    """A command's result: its exit code (None if it timed out), and its output."""

    exit_code: int | None
    stdout: str
    stderr: str


class Workspace(Protocol):
    """An open sandbox: paths are relative to its workspace, and cannot leave it."""

    async def list_files(self, path: str) -> list[str]: ...
    async def read_file(self, path: str) -> str: ...
    async def write_file(self, path: str, content: str) -> None: ...
    async def run(self, command: str, cwd: str, timeout: int) -> Ran: ...


class Provider(Protocol):
    """Where sandboxes are: one per name, opened again from the reference it gave."""

    async def open(self, name: str, ref: dict[str, Any] | None) -> tuple[dict[str, Any], Workspace]:
        """The sandbox ``name``: resumed from ``ref`` (its reference before), or made; and its reference now."""
        ...

    async def destroy(self, ref: dict[str, Any]) -> None:
        """Remove the sandbox of ``ref``, its files with it."""
        ...


class Sandbox:
    """
    A sandbox for each conversation of the agents whose tools are
    ``tools()``, on ``provider`` (``modal(...)`` and ``local()`` make one).
    """

    def __init__(self, provider: Provider) -> None:
        self.provider = provider
        # Each attempt's opening of its sandbox, by the attempt's context: one for its parallel tool calls.
        self._opened: weakref.WeakKeyDictionary[Any, asyncio.Task[Workspace]] = weakref.WeakKeyDictionary()

    def tools(self) -> list[Tool]:
        """The sandbox's tools: list, read and write files, and run a command."""
        return [tool(self.list_files), tool(self.read_file), tool(self.write_file), tool(self.run_command)]

    async def workspace(self, ctx: Any) -> Workspace:
        """The conversation's sandbox, opened once per attempt."""
        opening = self._opened.get(ctx)
        if opening is None or (opening.done() and opening.exception() is not None):
            opening = self._opened[ctx] = asyncio.ensure_future(self._open(ctx))
        return await asyncio.shield(opening)

    async def destroy(self, ctx: Any) -> None:
        """Remove the conversation's sandbox and its files (in a handler of your own, say)."""
        if (ref := await _stored(ctx.db, _name(ctx))) is not None:
            await self.provider.destroy(ref)
            await ctx.db.execute(f"DELETE FROM {TABLE} WHERE name = ?", [_name(ctx)])

    async def _open(self, ctx: Any) -> Workspace:
        name = _name(ctx)
        ref, workspace = await self.provider.open(name, await _stored(ctx.db, name))
        await _store(ctx.db, name, ref)
        return workspace

    async def list_files(self, ctx: Any, path: str = ".") -> list[str]:
        """List the files of a directory of the sandbox's workspace ('.' is the workspace); a directory ends in '/'."""
        return await (await self.workspace(ctx)).list_files(path)

    async def read_file(self, ctx: Any, path: str) -> str:
        """Read a text file of the sandbox's workspace."""
        return _clipped(await (await self.workspace(ctx)).read_file(path))

    async def write_file(self, ctx: Any, path: str, content: str) -> str:
        """Write a whole text file in the sandbox's workspace, its directories made if missing."""
        await (await self.workspace(ctx)).write_file(path, content)
        return f"Wrote {len(content.encode())} bytes to {path}"

    async def run_command(self, ctx: Any, command: str, cwd: str = ".", timeout_seconds: int = 120) -> str:
        """Run a shell command in the sandbox's workspace, and wait for it to end: its exit code and output."""
        ran = await (await self.workspace(ctx)).run(command, cwd, timeout_seconds)
        return json.dumps({"exit_code": ran.exit_code, "stdout": _clipped(ran.stdout), "stderr": _clipped(ran.stderr)})


def _name(ctx: Any) -> str:
    """The sandbox's name: the conversation's (its service and key), as a provider's names may be."""
    return "restate-" + hashlib.sha256(f"{service()}\0{ctx.key()}".encode()).hexdigest()[:32]


async def _stored(db: Any, name: str) -> dict[str, Any] | None:
    await SCHEMA.ensure(db)
    rows = await db.query(f"SELECT ref FROM {TABLE} WHERE name = ?", [name])
    return json.loads(rows[0][0]) if rows else None


async def _store(db: Any, name: str, ref: dict[str, Any]) -> None:
    await SCHEMA.ensure(db)
    await db.execute(f"INSERT OR REPLACE INTO {TABLE} VALUES (?, ?)", [name, json.dumps(ref)])


def _clipped(text: str) -> str:
    return text if len(text) <= CLIPPED else f"{text[:CLIPPED]}\n[{len(text) - CLIPPED} more characters]"


def _inside(root: str, path: str) -> str:
    """``path`` in the workspace at ``root``; one that leaves it is the tool's answer to the model."""
    target = posixpath.normpath(posixpath.join(root, path))
    if target != root and not target.startswith(root.rstrip("/") + "/"):
        raise restate.TerminalError(f"{path} is outside the sandbox's workspace")
    return target


def local(root: str | os.PathLike[str] | None = None) -> Sandbox:
    """Sandboxes in directories of this machine, below ``root`` (by default, the temporary directory's)."""
    return Sandbox(Local(Path(root) if root is not None else Path(tempfile.gettempdir()) / "restate-agents-sandboxes"))


@dataclass(frozen=True)
class Local:
    """Sandboxes in directories of this machine: for development, not isolation."""

    root: Path

    async def open(self, name: str, ref: dict[str, Any] | None) -> tuple[dict[str, Any], Workspace]:
        path = self.root / name
        path.mkdir(parents=True, exist_ok=True)
        return {"provider": "local", "path": str(path)}, LocalWorkspace(str(path))

    async def destroy(self, ref: dict[str, Any]) -> None:
        shutil.rmtree(ref["path"], ignore_errors=True)


@dataclass(frozen=True)
class LocalWorkspace:
    root: str

    async def list_files(self, path: str) -> list[str]:
        entries = sorted(Path(_inside(self.root, path)).iterdir())
        return [f"{e.name}/" if e.is_dir() else e.name for e in entries]

    async def read_file(self, path: str) -> str:
        try:
            return Path(_inside(self.root, path)).read_text()
        except FileNotFoundError:
            raise restate.TerminalError(f"no file {path}") from None

    async def write_file(self, path: str, content: str) -> None:
        target = Path(_inside(self.root, path))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    async def run(self, command: str, cwd: str, timeout: int) -> Ran:
        # A bare environment: the command does not see this process's API keys.
        env = {"PATH": os.environ.get("PATH", ""), "LANG": "C.UTF-8", "HOME": self.root, "TERM": "dumb"}
        process = await asyncio.create_subprocess_shell(
            command,
            cwd=_inside(self.root, cwd),
            env=env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            stdout, stderr = await asyncio.wait_for(process.communicate(), timeout)
        except TimeoutError:
            process.kill()
            return Ran(None, "", f"timed out after {timeout} s")
        return Ran(process.returncode, stdout.decode(errors="replace"), stderr.decode(errors="replace"))


# Where a Modal sandbox's workspace (its Volume) is mounted.
WORKDIR = "/workspace"


def modal(
    image: str = "python:3.13-slim", *, app: str = "restate-agents-sandboxes", idle_timeout: int = 600
) -> Sandbox:
    """
    Modal Sandboxes from the registry's ``image``, in the Modal app ``app``:
    each sandbox's files in a Volume of its own, mounted at /workspace; its
    compute stops after ``idle_timeout`` seconds without a command.
    """
    return Sandbox(Modal(image, app, idle_timeout))


@dataclass(frozen=True)
class Modal:
    """Modal Sandboxes, each with a Volume of its own for its files."""

    image: str
    app: str
    idle_timeout: int

    async def open(self, name: str, ref: dict[str, Any] | None) -> tuple[dict[str, Any], Workspace]:
        sandbox = await self._running(ref["sandbox"]) if ref else None
        if sandbox is None:
            sandbox = await self._create(name)
        return {"provider": "modal", "volume": name, "sandbox": sandbox.object_id}, ModalWorkspace(sandbox)

    async def destroy(self, ref: dict[str, Any]) -> None:
        import modal

        if (sandbox := await self._running(ref["sandbox"])) is not None:
            await sandbox.terminate.aio(wait=True)
        await modal.Volume.objects.delete.aio(ref["volume"], allow_missing=True)

    async def _running(self, sandbox_id: str) -> Any:
        """The sandbox ``sandbox_id``, if it still runs: Modal stops it once idle, and forgets it later."""
        import modal

        try:
            sandbox = await modal.Sandbox.from_id.aio(sandbox_id)
            return sandbox if await sandbox.poll.aio() is None else None
        except modal.exception.NotFoundError:
            return None

    async def _create(self, name: str) -> Any:
        """A sandbox named ``name``, on its Volume; the one a retried attempt made already, if it runs."""
        import modal

        app = await modal.App.lookup.aio(self.app, create_if_missing=True)
        volume = modal.Volume.from_name(name, create_if_missing=True)
        try:
            return await modal.Sandbox.create.aio(
                app=app,
                name=name,
                image=modal.Image.from_registry(self.image),
                volumes={WORKDIR: volume},
                workdir=WORKDIR,
                timeout=24 * 60 * 60,
                idle_timeout=self.idle_timeout,
            )
        except modal.exception.AlreadyExistsError:
            return await modal.Sandbox.from_name.aio(self.app, name)


@dataclass(frozen=True)
class ModalWorkspace:
    sandbox: Any

    async def list_files(self, path: str) -> list[str]:
        entries = await self.sandbox.filesystem.list_files.aio(_inside(WORKDIR, path))
        return sorted(f"{e.name}/" if e.type.value == "directory" else e.name for e in entries)

    async def read_file(self, path: str) -> str:
        import modal

        try:
            return await self.sandbox.filesystem.read_text.aio(_inside(WORKDIR, path))
        except (modal.exception.NotFoundError, FileNotFoundError):
            raise restate.TerminalError(f"no file {path}") from None

    async def write_file(self, path: str, content: str) -> None:
        await self.sandbox.filesystem.write_text.aio(content, _inside(WORKDIR, path))

    async def run(self, command: str, cwd: str, timeout: int) -> Ran:
        process = await self.sandbox.exec.aio("sh", "-lc", command, workdir=_inside(WORKDIR, cwd), timeout=timeout)
        stdout, stderr, exit_code = await asyncio.gather(
            process.stdout.read.aio(), process.stderr.read.aio(), process.wait.aio()
        )
        return Ran(exit_code, stdout, stderr)

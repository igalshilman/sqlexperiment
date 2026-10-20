"""
A Restate server and Python services, for the tests against a real server:
``restate-server`` from RESTATE_SERVER (the Makefile's test targets get it), on
the default ports, and services from files that end in ``serve(...)``, such
as the examples.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
INGRESS = "http://127.0.0.1:8080"
ADMIN = "http://127.0.0.1:9070"
# Requests go straight to localhost, whatever proxy the environment names.
_http = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def wait_until(done: Callable[[], bool], timeout: float = 60, what: str = "condition") -> None:
    deadline = time.monotonic() + timeout
    while not done():
        if time.monotonic() > deadline:
            raise TimeoutError(f"timed out waiting for {what}")
        time.sleep(0.1)


def listening(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def request(
    method: str, url: str, body: Any = None, headers: dict[str, str] | None = None, timeout: float = 30
) -> tuple[int, Any]:
    """The status and JSON body of a request; a status of 0 if it got no response."""
    data = None if body is None else json.dumps(body).encode()
    headers = dict(headers or {})
    if data is not None:
        headers["content-type"] = "application/json"
    try:
        with _http.open(urllib.request.Request(url, data, headers, method=method), timeout=timeout) as response:
            text = response.read().decode()
            return response.status, json.loads(text) if text else None
    except urllib.error.HTTPError as error:
        text = error.read().decode()
        try:
            return error.code, json.loads(text)
        except ValueError:
            return error.code, text
    except (OSError, urllib.error.URLError):
        return 0, None


class Server:
    """``restate-server`` in a directory of its own, killed and restarted at will."""

    def __init__(self, log: Path) -> None:
        self.binary = os.environ.get("RESTATE_SERVER") or str(ROOT / "target/restate/restate-server")
        self.dir = tempfile.mkdtemp(prefix="restate-python-tests-")
        self.log = log
        self.process: subprocess.Popen | None = None

    def start(self) -> None:
        env = {**os.environ, "RESTATE_BIND_IP": "127.0.0.1", "RESTATE_LISTEN_MODE": "tcp"}
        with open(self.log, "a") as log:
            self.process = subprocess.Popen(
                [self.binary, "--base-dir", self.dir], env=env, stdout=log, stderr=subprocess.STDOUT
            )
        wait_until(lambda: request("GET", f"{ADMIN}/health")[0] == 200, what="restate-server")

    def kill(self) -> None:
        if self.process is not None:
            self.process.send_signal(signal.SIGKILL)
            self.process.wait()
            self.process = None

    def close(self) -> None:
        """Kill it, and remove its directory."""
        self.kill()
        shutil.rmtree(self.dir, ignore_errors=True)


class Service:
    """A Python file of services (``args``: the file and its arguments), served on ``port`` with ``env``."""

    def __init__(self, args: list[Path], port: int, log: Path, env: dict[str, str] | None = None) -> None:
        self.args = [str(arg) for arg in args]
        self.name = Path(self.args[-1]).name
        self.port = port
        self.log = log
        self.env = env or {}
        self.process: subprocess.Popen | None = None
        self.starts = 0

    def start(self, extra_env: dict[str, str] | None = None) -> None:
        env = {**os.environ, **self.env, **(extra_env or {}), "PORT": str(self.port)}
        with open(self.log, "a") as log:
            self.process = subprocess.Popen([sys.executable, *self.args], env=env, stdout=log, stderr=subprocess.STDOUT)
        self.starts += 1
        process = self.process
        wait_until(lambda: listening(self.port) or process.poll() is not None, what=self.name)

    def alive(self) -> bool:
        return self.process is not None and self.process.poll() is None

    def stop(self) -> None:
        if self.alive():
            assert self.process is not None
            self.process.send_signal(signal.SIGKILL)
            self.process.wait()

    def register(self) -> None:
        status, body = request("POST", f"{ADMIN}/deployments", {"uri": f"http://127.0.0.1:{self.port}", "force": True})
        assert status in (200, 201), f"registering {self.name}: {status} {body}"


def call(path: str, body: Any = None, headers: dict[str, str] | None = None, timeout: float = 30) -> tuple[int, Any]:
    """A request to the ingress: ``path`` such as ``account/alice/balance``."""
    return request("POST", f"{INGRESS}/{path}", body, headers, timeout)

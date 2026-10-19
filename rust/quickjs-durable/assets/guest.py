"""The bridge between a durable Python program and the host.

It gives the program its `ctx`, and the host its entry points: `load` once, at
golden time, then `init`, `start`, `deliver`, `inspect` and `pause` in turns,
as for JavaScript (assets/guest.js). Between them no Python frame is on the
stack: the program's tasks wait in memory, on futures the host completes.

The program's coroutines run on an asyncio event loop that the host drains at
each turn. Its timers are durable sleeps, so `asyncio.sleep`, `asyncio.timeout`
and `asyncio.wait_for` survive crashes like `ctx.sleep` does.
"""

import asyncio
import collections
import gc
import inspect
import json
import random
import sys
import types
from asyncio import events
from string.templatelib import Interpolation, Template

import _durable


class OperationError(Exception):
    """A durable operation failed: `name` is the error's kind."""

    def __init__(self, name, message):
        super().__init__(message)
        self.name = name


class _Loop(asyncio.AbstractEventLoop):
    """The program's event loop: callbacks run when the host drains it.

    Timers are durable sleeps: the host fires them, perhaps in another process.
    """

    def __init__(self):
        self._ready = collections.deque()

    def call_soon(self, callback, *args, context=None):
        handle = asyncio.Handle(callback, args, self, context)
        self._ready.append(handle)
        return handle

    def call_later(self, delay, callback, *args, context=None):
        return self.call_at(self.time() + delay, callback, *args, context=context)

    def call_at(self, when, callback, *args, context=None):
        timer = asyncio.TimerHandle(when, callback, args, self, context)
        _bridge.sleep_until(round(when * 1000)).add_done_callback(
            lambda _: timer.cancelled() or self._ready.append(timer)
        )
        return timer

    def _timer_handle_cancelled(self, handle):
        pass

    def time(self):
        return _bridge.now / 1000

    def create_future(self):
        return asyncio.Future(loop=self)

    def create_task(self, coro, **kwargs):
        return asyncio.Task(coro, loop=self, **kwargs)

    def get_debug(self):
        return False

    def is_running(self):
        return True

    def is_closed(self):
        return False

    def call_exception_handler(self, context):
        # An error nobody awaited, as in a lost race: not the run's failure.
        pass

    def drain(self):
        events._set_running_loop(self)
        try:
            while self._ready:
                handle = self._ready.popleft()
                if not handle.cancelled():
                    handle._run()
        finally:
            events._set_running_loop(None)


def _plain(value):
    """`value` as plain JSON data, or a TypeError."""
    try:
        return json.loads(json.dumps(value, allow_nan=False, ensure_ascii=False))
    except (TypeError, ValueError) as error:
        raise TypeError(f"Expected JSON data: {error}") from None


def _failure(error):
    return {"ok": False, "error": {"name": type(error).__name__, "message": str(error)}}


def _sql_param(value):
    if value is None or isinstance(value, (bool, float, str)):
        return value
    if isinstance(value, int):
        return value if -(2**53) < value < 2**53 else {"$int": str(value)}
    if isinstance(value, (bytes, bytearray, memoryview)):
        return {"$blob": bytes(value).hex()}
    raise TypeError("An SQL parameter is None, a bool, int, float, str or bytes")


def _sql_value(value):
    if not isinstance(value, dict):
        return value
    if "$int" in value:
        return int(value["$int"])
    return bytes.fromhex(value["$blob"])


class _Transaction:
    def __init__(self, db):
        self._db = db

    def __enter__(self):
        self._db._host("db.begin", None)

    def __exit__(self, kind, error, traceback):
        self._db._host("db.rollback" if kind else "db.commit", None)
        return False


class Db:
    """The scope's SQL database. Statements are synchronous, and commit with
    the turn."""

    def __init__(self, bridge):
        self._bridge = bridge

    def _host(self, operation, request):
        return self._bridge.host(operation, request)

    def sql(self, query, /, *params):
        """Run one statement; its rows, as dicts.

            ctx.db.sql(t"SELECT text FROM notes WHERE id = {id}")
            ctx.db.sql("SELECT text FROM notes WHERE id = ?", id)

        A template's interpolations are parameters, never SQL text.
        """
        if isinstance(query, Template):
            if params:
                raise TypeError("A template carries its own parameters")
            parts = []
            for part in query:
                if isinstance(part, Interpolation):
                    parts.append("?")
                    params += (part.value,)
                else:
                    parts.append(part)
            query = "".join(parts)
        if not isinstance(query, str):
            raise TypeError('SQL is a string or a template: ctx.db.sql(t"SELECT ...")')
        rows = self._host("db.sql", {"sql": query, "params": [_sql_param(p) for p in params]})
        return [{column: _sql_value(value) for column, value in row.items()} for row in rows]

    def transaction(self):
        """Statements in `with ctx.db.transaction():` stand together, or not
        at all if the block raises."""
        return _Transaction(self)


class Kv:
    """The scope's durable key-value state."""

    def __init__(self, bridge):
        self._bridge = bridge

    def get(self, key):
        return self._bridge.request({"kind": "kv.get", "key": _key(key)})

    def set(self, key, value):
        return self._bridge.request({"kind": "kv.set", "key": _key(key), "value": _plain(value)})

    def delete(self, key):
        return self._bridge.request({"kind": "kv.delete", "key": _key(key)})


def _key(key):
    if not isinstance(key, str):
        raise TypeError("A KV key is a string")
    return key


class Rpc:
    """Calls to Restate services."""

    def __init__(self, bridge):
        self._bridge = bridge

    def call(self, service, handler, input=None, *, key=None):
        """Call `service`'s `handler` (on object `key`, if given) with `input`;
        its answer."""
        if not (isinstance(service, str) and service and isinstance(handler, str) and handler):
            raise TypeError("A call names a service and a handler")
        target = {"service": service, "handler": handler}
        if key is not None:
            target["key"] = _key(key)
        return self._bridge.request({"kind": "rpc", "target": target, "input": _plain(input)})


class Context:
    """What a program's handlers get, as `ctx`: their scope's state, and their
    durable operations. Each returns an awaitable; await several at once with
    `asyncio.gather`."""

    def __init__(self, bridge):
        self._bridge = bridge
        self.db = Db(bridge)
        self.kv = Kv(bridge)
        self.rpc = Rpc(bridge)

    @property
    def key(self):
        """The scope: the key of the object this program is served as."""
        return self._bridge.scope

    def call(self, name, args=None):
        """The host's operation `name` (a model call, a tool), with `args`."""
        if not isinstance(name, str) or not name:
            raise TypeError("An operation name is a nonempty string")
        return self._bridge.request({"kind": "call", "name": name, "args": _plain(args)})

    async def sleep(self, seconds):
        """Wait `seconds`, durably (as `asyncio.sleep` does here)."""
        await asyncio.sleep(seconds)


class _Bridge:
    def __init__(self):
        self.exports = {}
        self.loop = _Loop()
        self.run_id = None
        self.scope = None
        self.now = 0
        self.max_time = 0
        self.next_operation = 0
        # Each request's future, by ID, until delivered. The request itself
        # goes to the host at the next report, and is forgotten here.
        self.pending = {}
        self.issued = []
        self.outcome = None

    def host(self, operation, request):
        if self.run_id is None:
            raise RuntimeError("The database is unavailable while the program loads")
        answer = json.loads(_durable.host(operation, json.dumps(request, ensure_ascii=False)))
        if answer["ok"]:
            return answer["value"]
        raise OperationError(answer["error"]["name"], answer["error"]["message"])

    def request(self, operation):
        if self.run_id is None:
            raise RuntimeError("Durable operations are unavailable while the program loads")
        id = f"{self.run_id}/{self.next_operation}"
        self.next_operation += 1
        self.issued.append({"id": id, **operation})
        future = self.loop.create_future()
        self.pending[id] = future
        return future

    def sleep_until(self, deadline):
        if not self.now <= deadline <= self.max_time:
            raise ValueError("Invalid sleep duration")
        return self.request({"kind": "sleep", "deadline": deadline})

    # The host's entry points.

    def load(self, program):
        program = json.loads(program)
        module = types.ModuleType(program["name"])
        module.__file__ = f"{program['name']}.py"
        sys.modules[program["name"]] = module
        exec(compile(program["source"], module.__file__, "exec"), module.__dict__)
        # Its handlers: the coroutine functions it defines, but not those whose
        # names start with an underscore, nor those it imports.
        self.exports = {
            name: value
            for name, value in vars(module).items()
            if not name.startswith("_")
            and inspect.iscoroutinefunction(value)
            and value.__module__ == module.__name__
        }
        # Everything loaded so far lives as long as the program: out of the
        # collector's sight, so a collection in a turn does not visit it, nor
        # write to the pages it is on.
        gc.collect()
        gc.freeze()

    def pause(self):
        self.request({"kind": "resume"})

    def init(self, config):
        if self.run_id is not None:
            raise RuntimeError("Invocation is already initialized")
        config = json.loads(config)
        self.run_id = config["runId"]
        self.scope = config["scope"]
        self.now = config["now"]
        self.max_time = config["maxTime"]
        random.seed(config.get("seed") or 1)

    def handlers(self):
        return json.dumps(sorted(self.exports))

    def start(self, call):
        call = json.loads(call)
        handler = self.exports.get(call["handler"])
        if handler is None:
            self.outcome = _failure(TypeError(f"No handler {call['handler']}"))
            return
        task = self.loop.create_task(handler(call["input"], Context(self)))

        def settled(task):
            if task.cancelled():
                self.outcome = _failure(asyncio.CancelledError("The handler was cancelled"))
            elif task.exception() is not None:
                self.outcome = _failure(task.exception())
            else:
                try:
                    self.outcome = {"ok": True, "value": _plain(task.result())}
                except TypeError as error:
                    self.outcome = _failure(error)

        task.add_done_callback(settled)

    def deliver(self, event):
        event = json.loads(event)
        self.now = max(self.now, event["now"])
        future = self.pending.pop(event["operationId"], None)
        if future is None:
            raise RuntimeError("Completion has no pending future")
        if future.cancelled():
            return
        outcome = event["outcome"]
        if outcome["ok"]:
            future.set_result(outcome["value"])
        else:
            future.set_exception(OperationError(outcome["error"]["name"], outcome["error"]["message"]))

    def drain(self):
        self.loop.drain()

    def inspect(self):
        if self.outcome is None and not self.pending:
            self.outcome = _failure(RuntimeError("Execution stalled: no durable wakeup"))
        # The run ends when its handler settles: requests still pending are
        # dropped, and their continuations never run.
        if self.outcome is not None:
            self.pending.clear()
            self.issued = []
        report = {
            "issued": self.issued,
            "nextOperation": self.next_operation,
            "now": self.now,
            "status": "waiting" if self.outcome is None else "completed" if self.outcome["ok"] else "failed",
        }
        if self.outcome is not None:
            report["outcome"] = self.outcome
        self.issued = []
        return json.dumps(report, ensure_ascii=False)


_bridge = _Bridge()
__durable = _bridge

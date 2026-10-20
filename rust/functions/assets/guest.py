"""The bridge between a durable Python program and the host.

It gives the program its `ctx`, and the host its entry points: `load` once, at
golden time, then `init`, `start`, `deliver`, `inspect` and `pause` in turns,
as for JavaScript (assets/guest.js). Between them no Python frame is on the
stack: the program's tasks wait in memory, on futures the host completes.

The program's coroutines run on an asyncio event loop that the host drains at
each turn. Its timers are durable sleeps, so `asyncio.sleep`, `asyncio.timeout`
and `asyncio.wait_for` survive crashes like `ctx.sleep` does.
"""

import collections.abc
import gc
import importlib.util
import json
import random
import sys
import types
from string.templatelib import Interpolation, Template

import _durable


class _Placeholder(types.ModuleType):
    """A module asyncio's core imports for what a run seldom or never does
    (sockets, subprocesses, thread pools, formatting stacks): it holds the few
    names asyncio reads as it loads, and imports the real module on any other
    use. Programs importing the module get the real one."""

    def __init__(self, name, **constants):
        super().__init__(name)
        self.__dict__.update(constants)

    def __getattr__(self, name):
        if sys.modules.get(self.__name__) is self:
            raise AttributeError(f"{self.__name__}.{name} is used while asyncio loads")
        return getattr(_real(self.__name__), name)


def _real(name):
    return sys.modules.get(name) or importlib.import_module(name)


def _extract_stack(*args, **kwargs):
    return _real("traceback").extract_stack(*args, **kwargs)


def _import_asyncio_core():
    """Import asyncio's core: events, futures, tasks, locks, queues, timeouts
    and task groups. The rest (streams, subprocesses, threads, `asyncio.run`)
    loads on first use. Every program's image is 2.8 MB smaller."""
    placeholders = [
        _Placeholder("socket", AF_UNSPEC=0, AI_PASSIVE=1),
        _Placeholder("subprocess", PIPE=-1),
        _Placeholder("signal"),
        _Placeholder("logging", DEBUG=10),
        _Placeholder("inspect"),
        # The C half of asyncio keeps this function, for debug mode.
        _Placeholder("traceback", extract_stack=_extract_stack),
        _Placeholder(
            "concurrent.futures",
            FIRST_COMPLETED="FIRST_COMPLETED",
            FIRST_EXCEPTION="FIRST_EXCEPTION",
            ALL_COMPLETED="ALL_COMPLETED",
        ),
    ]
    import concurrent

    for placeholder in placeholders:
        sys.modules[placeholder.__name__] = placeholder
    concurrent.futures = placeholders[-1]

    spec = importlib.util.find_spec("asyncio")
    package = importlib.util.module_from_spec(spec)
    sys.modules["asyncio"] = package
    try:
        for name in (
            "exceptions",
            "events",
            "futures",
            "tasks",
            "locks",
            "queues",
            "timeouts",
            "taskgroups",
            "coroutines",
        ):
            module = importlib.import_module(f"asyncio.{name}")
            package.__dict__.update((export, getattr(module, export)) for export in module.__all__)
    finally:
        for placeholder in placeholders:
            del sys.modules[placeholder.__name__]
        del concurrent.futures

    def load_the_rest(name):
        # asyncio's own __init__, run over the package: it imports the rest,
        # and defines asyncio's own __getattr__ in place of this one.
        del package.__getattr__
        spec.loader.exec_module(package)
        return getattr(package, name)

    package.__getattr__ = load_the_rest
    return package


asyncio = _import_asyncio_core()


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
        asyncio.events._set_running_loop(self)
        try:
            while self._ready:
                handle = self._ready.popleft()
                if not handle.cancelled():
                    try:
                        handle._run()
                    except (SystemExit, KeyboardInterrupt):
                        # A task raised it: the task holds it as its error,
                        # and if it is main's, the run fails with it.
                        pass
        finally:
            asyncio.events._set_running_loop(None)


# What crosses to the host: JSON, read there as UTF-8, with numbers as
# doubles, and at most a megabyte.
_MAX_INT = 2**53
_MAX_BYTES = 1024 * 1024


class _Encoder(json.JSONEncoder):
    def iterencode(self, value, _one_shot=False):
        return super().iterencode(_checked(value), _one_shot)


def _checked(value):
    """`value`, with an int a double cannot hold refused."""
    if isinstance(value, bool) or value is None:
        return value
    if isinstance(value, int):
        if not -_MAX_INT < value < _MAX_INT:
            raise ValueError(f"{value} is beyond 2**53: send it as a string")
        return value
    if isinstance(value, dict):
        return {key: _checked(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_checked(item) for item in value]
    return value


def _json(value):
    """`value` as JSON text, or a TypeError."""
    try:
        text = json.dumps(value, allow_nan=False, ensure_ascii=False, cls=_Encoder)
        size = len(text.encode())
    except (TypeError, ValueError, UnicodeEncodeError) as error:
        raise TypeError(f"Expected JSON data: {error}") from None
    if size > _MAX_BYTES:
        raise TypeError(f"Expected JSON data of at most 1 MiB, not {size} bytes")
    return text


def _plain(value):
    """`value` as plain JSON data, or a TypeError."""
    return json.loads(_json(value))


def _idempotency(key):
    """A call's idempotency key of the program's own, if any."""
    if key is None:
        return {}
    if not isinstance(key, str) or not key:
        raise TypeError("An idempotency key is a nonempty string")
    return {"idempotencyKey": key}


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

    async def get(self, key):
        return await self._bridge.request({"kind": "kv.get", "key": _key(key)})

    async def set(self, key, value):
        return await self._bridge.request({"kind": "kv.set", "key": _key(key), "value": _plain(value)})

    async def delete(self, key):
        return await self._bridge.request({"kind": "kv.delete", "key": _key(key)})


def _key(key):
    if not isinstance(key, str):
        raise TypeError("A KV key is a string")
    return key


class Conversation:
    """A conversation with a model, kept in the scope's database: `send` adds
    messages, and the host reads them all when it asks the model."""

    def __init__(self, bridge, name, defaults):
        self._bridge = bridge
        self.name = name
        self._defaults = defaults

    def _host(self, operation, **request):
        return self._bridge.host(operation, {"conversation": self.name, **request})

    async def send(self, *messages, **params):
        """Add `messages`, ask the model with the whole conversation and
        `params` (over the conversation's defaults), and add its answer:
        its response."""
        if self.name in self._bridge.asking:
            raise RuntimeError(f"The conversation {self.name} is waiting for the model")
        for message in messages:
            if not isinstance(message, dict) or message.get("role") not in ("user", "assistant"):
                raise TypeError("A message is a dict whose role is user or assistant")
        self._host("llm.append", messages=_plain(list(messages)))
        self._bridge.asking.add(self.name)
        try:
            asked = {**self._defaults, **_plain(params), "conversation": self.name}
            response = await self._bridge.request({"kind": "call", "name": "llm"}, asked)
            self._host("llm.append", messages=[{"role": "assistant", "content": response["content"]}])
            return response
        finally:
            self._bridge.asking.discard(self.name)

    def history(self):
        """The messages so far."""
        return self._host("llm.history")

    def clear(self):
        """Forget the messages so far."""
        if self.name in self._bridge.asking:
            raise RuntimeError(f"The conversation {self.name} is waiting for the model")
        self._host("llm.clear")


class Llm:
    """Models, through the host's gateway: any provider's, in one format (the
    Anthropic Messages API's), named `<provider>/<model>`."""

    def __init__(self, bridge):
        self._bridge = bridge

    async def complete(self, request):
        """Send a whole request, `{"model", "messages", ...}`: its response."""
        if not isinstance(request, dict) or "conversation" in request:
            raise TypeError("A model request is a dict: {model, messages, ...}")
        return await self._bridge.request({"kind": "call", "name": "llm"}, request)

    def conversations(self):
        """The scope's conversations' names, in the order they began."""
        return self._bridge.host("llm.list", None)

    def conversation(self, name, **defaults):
        """The conversation `name` of this scope; `defaults` (model, system,
        tools...) go with each request."""
        if not isinstance(name, str) or not name:
            raise TypeError("A conversation has a name")
        if "messages" in defaults:
            raise TypeError("A conversation's defaults have no messages")
        return Conversation(self._bridge, name, _plain(defaults))


class Rpc:
    """Calls to Restate services."""

    def __init__(self, bridge):
        self._bridge = bridge

    async def call(self, service, handler, input=None, *, key=None, idempotency_key=None):
        """Call `service`'s `handler` (on object `key`, if given) with `input`;
        its answer. `input` is read when the call is made, and again after a
        crash: do not change it until the call is answered."""
        if not (isinstance(service, str) and service and isinstance(handler, str) and handler):
            raise TypeError("A call names a service and a handler")
        target = {"service": service, "handler": handler}
        if key is not None:
            target["key"] = _key(key)
        operation = {"kind": "rpc", "target": target, **_idempotency(idempotency_key)}
        return await self._bridge.request(operation, input)


class Response:
    """An HTTP response, as `ctx.fetch` returns it: its status, headers and
    body, all in memory."""

    def __init__(self, answer):
        self.status_code = answer["status"]
        self.reason_phrase = answer["statusText"]
        self.url = answer["url"]
        self.redirected = answer["redirected"]
        # Each header as it came, a name (lowercase) and a value.
        self.header_list = [(name, value) for name, value in answer["headers"]]
        if "bodyBase64" in answer:
            import base64

            self.content = base64.b64decode(answer["bodyBase64"])
        else:
            self.content = answer["body"].encode()

    @property
    def is_success(self):
        return 200 <= self.status_code <= 299

    @property
    def headers(self):
        """The headers as a dict, by lowercase name; repeated ones joined."""
        headers = {}
        for name, value in self.header_list:
            headers[name] = f"{headers[name]}, {value}" if name in headers else value
        return headers

    @property
    def text(self):
        return self.content.decode(errors="replace")

    def json(self):
        return json.loads(self.content)

    def __repr__(self):
        return f"<Response [{self.status_code}]>"


class Context:
    """What a program's handlers get, as `ctx`: their scope's state, and their
    durable operations. Each is a coroutine: await it, or run several at once
    with `asyncio.gather` or a task group."""

    def __init__(self, bridge):
        self._bridge = bridge
        self.db = Db(bridge)
        self.kv = Kv(bridge)
        self.rpc = Rpc(bridge)
        self.llm = Llm(bridge)

    @property
    def key(self):
        """The scope: the key of the object this program is served as."""
        return self._bridge.scope

    async def call(self, name, input=None, *, idempotency_key=None):
        """The host binding `name`, such as `fetch`, with `input`,
        which is read as an RPC's is."""
        if not isinstance(name, str) or not name:
            raise TypeError("A binding name is a nonempty string")
        operation = {"kind": "call", "name": name, **_idempotency(idempotency_key)}
        return await self._bridge.request(operation, input)

    async def fetch(self, url, *, method="GET", headers=None, body=None, json=None, redirect="follow"):
        """Send an HTTP request, and return its `Response`. The host sends it,
        under its policy: only to origins it allows, adding the secrets it
        keeps (such as an API key), and with no cookies kept between
        requests. `body` is text or bytes; or `json` is data the host sends
        as JSON, so that the program keeps no copy of it as text. A request
        is sent at least once: again after a crash, as it was. Do not change
        `json` until the response is in."""
        if not isinstance(url, str):
            raise TypeError("A URL is a string")
        method = method.upper() if isinstance(method, str) else method
        if not isinstance(method, str) or not method:
            raise TypeError("A method is a nonempty string")
        request = {"url": url, "method": method, "redirect": redirect}
        request["headers"] = [[str(name).lower(), str(value)] for name, value in dict(headers or {}).items()]
        if redirect not in ("follow", "manual", "error"):
            raise TypeError('redirect is "follow", "manual" or "error"')
        if body is not None and json is not None:
            raise TypeError("A request has a body or json, not both")
        if json is not None:
            if method in ("GET", "HEAD"):
                raise TypeError(f"A {method} request has no body")
            request["json"] = json
            if not any(name == "content-type" for name, _ in request["headers"]):
                request["headers"].append(["content-type", "application/json"])
        if body is not None:
            if method in ("GET", "HEAD"):
                raise TypeError(f"A {method} request has no body")
            if isinstance(body, str):
                request["body"] = body
            elif isinstance(body, (bytes, bytearray, memoryview)):
                import base64

                request["bodyBase64"] = base64.b64encode(body).decode()
            else:
                raise TypeError("A request body is text or bytes: json.dumps a dict")
        return Response(await self._bridge.request({"kind": "call", "name": "fetch"}, request))

    async def signal(self, name, input=None):
        """Wait for the signal `name`, which the object's `signal` handler gives
        (by its wait's id, or to the oldest wait for its name), and return its
        value. `input` says what it waits for, as `pending` lists it."""
        if not isinstance(name, str) or not name:
            raise TypeError("A signal name is a nonempty string")
        return await self._bridge.request({"kind": "signal", "name": name, "input": _plain(input)})

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
        # Each request's future, by ID, until delivered. The request goes to
        # the host at the next report, and is forgotten here, but for a call's
        # input: it stays, for the host to read when it makes the call.
        self.pending = {}
        self.inputs = {}
        # The conversations waiting for the model: one call at a time each.
        self.asking = set()
        self.issued = []
        self.outcome = None

    def host(self, operation, request):
        if self.run_id is None:
            raise RuntimeError("The database is unavailable while the program loads")
        answer = json.loads(_durable.host(operation, json.dumps(request, ensure_ascii=False)))
        if answer["ok"]:
            return answer["value"]
        raise OperationError(answer["error"]["name"], answer["error"]["message"])

    def request(self, operation, input=None):
        if self.run_id is None:
            raise RuntimeError("Durable operations are unavailable while the program loads")
        id = f"{self.run_id}/{self.next_operation}"
        self.next_operation += 1
        self.issued.append({"id": id, **operation})
        future = self.loop.create_future()
        self.pending[id] = future
        if operation["kind"] in ("rpc", "call"):
            self.inputs[id] = input
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
        # Its handlers: the functions it names in __all__, or else every
        # function it defines (not imports) whose name has no leading _.
        names = getattr(module, "__all__", None)
        self.exports = {
            name: value
            for name, value in vars(module).items()
            if isinstance(value, types.FunctionType)
            and value.__module__ == module.__name__
            and (name in names if names is not None else not name.startswith("_"))
        }
        # Everything loaded so far lives as long as the program: out of the
        # collector's sight, so a collection in a turn does not visit it, nor
        # write to the pages it is on.
        gc.collect()
        gc.freeze()

    def pause(self):
        self.request({"kind": "resume"})

    def input(self, id):
        """The input of the pending call `id`, as an outcome: its JSON, or why
        it has none."""
        try:
            if id not in self.inputs:
                raise RuntimeError(f"No pending call {id}")
            return '{"ok":true,"value":' + _json(self.inputs[id]) + "}"
        except Exception as error:
            return json.dumps(_failure(error), ensure_ascii=False)

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

        async def run():
            result = handler(call["input"], Context(self))
            return await result if isinstance(result, collections.abc.Awaitable) else result

        task = self.loop.create_task(run())

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
        self.inputs.pop(event["operationId"], None)
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
            self.inputs.clear()
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

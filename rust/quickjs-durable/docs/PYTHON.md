# Durable Python

CPython runs as the second engine, next to QuickJS. A `<name>.py` in
[`examples-functions-python/`](../../../examples-functions-python/README.md)
is served like a `<name>.js`: a virtual object, whose handlers are the
functions the module defines, but those whose names start with `_` (or those
its `__all__` names). At each wait the runtime
saves the interpreter's memory: coroutines, frames, objects. When a result
arrives, or after a crash, it restores that memory. Nothing is replayed.

```python
# examples-functions-python/counter.py: the virtual object `counter`
async def add(n, ctx):
    count = (await ctx.kv.get("count") or 0) + n
    await ctx.kv.set("count", count)
    await ctx.sleep(1)
    return count


async def get(_, ctx):
    return await ctx.kv.get("count") or 0
```

Everything below the engine is shared with JavaScript: turns, commits, write
tracking, encoding, the Restate binding and the database. This page covers
what is Python's own.

## The engine

`make python-engine` (`python/build.sh`) builds CPython 3.15 for WASI with
wasi-sdk, as CPython's own `Platforms/WASI` script does. It then links
CPython's library with [`python/reactor.c`](../python/reactor.c) instead of a
`main`, into a reactor. The result goes to `target/pywasm/out`:

| File          | What                                                                                                                                                                |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `python.wasm` | 8 MB: the interpreter and its built-in modules. A 2 MB stack placed first in memory, so that it overflows into a trap. `__stack_pointer` as the only mutable global |
| `lib/`        | The standard library (27 MB), without tests or GUI toolkits, with bytecode checked by its source's hash                                                             |

The reactor's call surface is the one [ENGINES.md](ENGINES.md) asks for:

| Export                           | What                                                                                             |
| -------------------------------- | ------------------------------------------------------------------------------------------------ |
| `py_init()`                      | Start the interpreter: isolated, hash seed 0, no site packages, the library at `/lib/python3.15` |
| `py_exec(source, name)`          | Run source in `__main__`: the bridge, once                                                       |
| `py_call(name, argument)`        | Call `__durable.<name>(argument)` with a string, and keep the string it returns                  |
| `py_result()`, `py_result_len()` | That string, or the traceback of the error                                                       |
| `py_alloc(len)`, `py_free(ptr)`  | Memory for the host's arguments                                                                  |

Python reaches the host through one import, `durable.host(operation,
request)`. It is exposed as the built-in module `_durable` and answers
database calls synchronously, as `__host` does for JavaScript. The answer
comes back in two steps: `host` returns its length, then `host_read` copies it
in. No host call re-enters the guest.

## The bridge

[`assets/guest.py`](../assets/guest.py) gives the program its `ctx`. It speaks
the protocol of `guest.js` (`handlers`, `init`, `start`, `deliver`, `inspect`, `pause`),
so the run loop cannot tell the two languages apart. It also gives the
program an asyncio event loop that the host drains each turn:

- **Tasks are asyncio's own** (`asyncio.Task`, `Future`, `gather`, `wait`,
  `TaskGroup`). A pending request is a future, completed by `deliver`.
- **Timers are durable sleeps.** The loop's `call_at` issues a sleep request,
  so `asyncio.sleep`, `asyncio.timeout` and `asyncio.wait_for` survive crashes
  like `ctx.sleep` does. Time is the turn's logical clock.
- **After the program loads,** `gc.freeze()` moves everything loaded
  (modules, classes, the bridge) out of the collector's sight. A collection
  in a turn then neither visits those objects nor writes to their pages. A
  turn changes a quarter fewer pages, and under heap churn two thirds fewer.

`ctx` is the JavaScript `ctx`, written the Python way:

| JavaScript                                        | Python                                                                            |
| ------------------------------------------------- | --------------------------------------------------------------------------------- |
| ``ctx.db.sql`SELECT ... ${value}` ``              | `ctx.db.sql(t"SELECT ... {value}")` (a t-string), or `ctx.db.sql("... ?", value)` |
| `ctx.db.transaction(fn)`                          | `with ctx.db.transaction(): ...`                                                  |
| `ctx.kv.get/set/delete(key)`                      | `await ctx.kv.get/set/delete(key)`                                                |
| `ctx.rpc.call({ service, handler, key? }, input)` | `await ctx.rpc.call(service, handler, input, key=...)`                            |
| `ctx.sleep(ms)`                                   | `await ctx.sleep(seconds)`, or `asyncio.sleep(seconds)`                           |
| `Promise.all`, `Promise.race`                     | `asyncio.gather`, `asyncio.wait`, `asyncio.timeout`                               |

## Its WASI

[`src/wasi.rs`](../src/wasi.rs) is what CPython sees of a system. QuickJS
needs a clock, randomness and stdout; CPython also needs a filesystem to
import from:

- **A read-only tree in memory:** the standard library, read when the engine
  loads. Every file reports the same times, and inodes number the files in
  order. Evaluating a program therefore reads the same metadata in every
  process, and its golden image is the same byte for byte (tested).
- **Open files are host state**, which no image keeps. A turn that ends with
  a file open fails, rather than restoring a VM whose descriptors are gone.
- **stdout and stderr** go to the host's stderr: a program's `print` shows in
  the server's log.
- **Randomness** comes from the operating system after golden evaluation
  (`os.urandom`, `uuid4`, `secrets`); memory is saved, not replayed, so
  nothing needs to repeat. `random` is seeded per run, as `Math.random` is.
- **No sockets, writes, subprocesses or blocking sleeps.** All I/O goes through
  `ctx`.

## What it costs

`cargo bench --bench languages` runs the same workloads in both languages.
These are figures from one machine, against a store in memory:

| Metric                                                 | JavaScript | Python      |
| ------------------------------------------------------ | ---------- | ----------- |
| Golden image (memory)                                  | 1.4 MB     | 11.4 MB     |
| Evaluating a program at startup                        | ~3 ms      | ~110–210 ms |
| A turn on a live VM (`scalar`)                         | 0.3–0.7 ms | 1.2–2.4 ms  |
| A turn on a restored VM (`scalar`)                     | 1.6–2.1 ms | 8.6–12.8 ms |
| Pages a turn changes (`scalar`)                        | 18–39      | 66–109      |
| Pages a turn changes, 2,000 new objects (`heap-churn`) | 107–169    | 225–323     |
| Batch a turn writes to Restate (`heap-churn`)          | 52–56 KB   | 57–62 KB    |

What a turn writes to Restate is about the same in both languages, and that
dominates throughput ([ENGINES.md](ENGINES.md)). Restoring a VM costs more in
Python, because the runtime writes the whole 11 MB image into a fresh
instance. That happens once per attempt, not once per turn.

Three changes brought these figures down from the first build:

- **Bytecode for the standard library.** Without it, every program's
  evaluation compiled asyncio and its imports from source: about 1 s.
- **A cached compiled module.** Cranelift takes ~8 s to compile CPython. The
  runtime serializes the module next to the engine, keyed by the engine's
  configuration and the Wasm, and loads it from there afterwards.
- **`gc.freeze()` after load.** Pages changed per turn fell from 85–147 to
  66–109, and from 680–1,190 to 225–323 under heap churn. Batches shrank by
  half.

## Next

- **A shared base image.** Every program evaluates the interpreter, the
  bridge and asyncio again (~100 ms). One image of those, restored before
  each program loads, would leave only the program to evaluate.
- **Cheaper restores.** The image could become the module's data segment, as
  Wizer does, so that Wasmtime maps it copy-on-write instead of writing
  11 MB. That needs native write tracking to coexist with the
  copy-on-write mapping, which is why `memory_init_cow` is off today.
- **Dead heap bytes.** The heap walker zeroes free dlmalloc chunks, but most
  Python objects live in pymalloc's arenas, which are always in use from
  dlmalloc's point of view. Walking pymalloc's free blocks too would keep
  freed objects out of the batches.
- **Packages.** A program can import the standard library only. Pure-Python
  packages could be added to the tree; packages with C extensions would need
  building for WASI and linking into the reactor.

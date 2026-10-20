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

| File          | What                                                                                                                                                                                                    |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `python.wasm` | 8 MB: the interpreter and its built-in modules, with zlib and the C decimal module. A 2 MB stack placed first in memory, so that it overflows into a trap. `__stack_pointer` as the only mutable global |
| `lib/`        | The standard library (27 MB), without tests or GUI toolkits, with bytecode checked by its source's hash; the build's sysconfig data, and the `tzdata` package for `zoneinfo`                            |

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
the protocol of `guest.js` (`handlers`, `init`, `start`, `deliver`, `inspect`,
`pause`), so the run loop cannot tell the two languages apart. A module's
handlers are the functions it defines (not imports) whose names do not start
with `_`, or those its `__all__` names; they may be sync or async. The bridge
also gives the program an asyncio event loop that the host drains each turn:

- **Tasks are asyncio's own** (`asyncio.Task`, `Future`, `gather`, `wait`,
  `TaskGroup`). A pending request is a future, completed by `deliver`, and
  `ctx`'s operations are coroutines, so tasks and task groups take them.
- **Timers are durable sleeps.** The loop's `call_at` issues a sleep request,
  so `asyncio.sleep`, `asyncio.timeout` and `asyncio.wait_for` survive crashes
  like `ctx.sleep` does. Time is the turn's logical clock.
- **After the program loads,** `gc.freeze()` moves everything loaded
  (modules, classes, the bridge) out of the collector's sight. A collection
  in a turn then neither visits those objects nor writes to their pages. A
  turn changes a quarter fewer pages, and under heap churn two thirds fewer.
- **asyncio loads its core only:** events, futures, tasks, locks, queues,
  timeouts and task groups. Streams, subprocesses, threads and `asyncio.run`
  load on first use, as do the modules the core imports only for them or for
  formatting errors (`socket`, `subprocess`, `logging`, `concurrent.futures`,
  `inspect`, `traceback`): placeholders stand in for them while asyncio
  loads, and a program that imports one gets the real module.

`ctx` is the JavaScript `ctx`, written the Python way:

| JavaScript                                        | Python                                                                            |
| ------------------------------------------------- | --------------------------------------------------------------------------------- |
| ``ctx.db.sql`SELECT ... ${value}` ``              | `ctx.db.sql(t"SELECT ... {value}")` (a t-string), or `ctx.db.sql("... ?", value)` |
| `ctx.db.transaction(fn)`                          | `with ctx.db.transaction(): ...`                                                  |
| `ctx.kv.get/set/delete(key)`                      | `await ctx.kv.get/set/delete(key)`                                                |
| `ctx.rpc.call({ service, handler, key? }, input)` | `await ctx.rpc.call(service, handler, input, key=...)`                            |
| `ctx.sleep(ms)`                                   | `await ctx.sleep(seconds)`, or `asyncio.sleep(seconds)`                           |
| `ctx.signal(name, input)`                         | `await ctx.signal(name, input)`                                                   |
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
  a file open fails the run, rather than restoring a VM whose descriptors are
  gone.
- **stdout and stderr** go to the host's stderr: a program's `print` shows in
  the server's log.
- **Randomness** comes from the operating system after golden evaluation
  (`os.urandom`, `uuid4`, `secrets`); memory is saved, not replayed, so
  nothing needs to repeat. `random` is seeded per run, as `Math.random` is.
- **No sockets, writes, subprocesses, threads or blocking sleeps.** Each
  raises as Python would (`OSError`, `RuntimeError`), where the program can
  catch it. All I/O goes through `ctx`.

## At the edges

`tests/python.rs` and the crash suite (`tests/crashes.rs`, which runs a Python
twin of its flow: a crash at every write, and during the recovery from one)
pin down what happens where a Python program meets the runtime's limits:

| A program that                                               | Gets                                                                     |
| ------------------------------------------------------------ | ------------------------------------------------------------------------ |
| Recurses deeply in Python                                    | `RecursionError` (the usual limit of 1,000 frames)                       |
| Recurses deeply in C (`json.dumps`, `repr` of nested lists)  | `RecursionError`: see below                                              |
| Allocates past 64 MiB                                        | `MemoryError`, and the run goes on                                       |
| Loops without waiting past the turn's limit                  | The run fails: "Out of time"                                             |
| Calls `sys.exit`, or raises `KeyboardInterrupt`              | The run fails with `SystemExit` or `KeyboardInterrupt`                   |
| Sends an int beyond 2\*\*53, a lone surrogate, or over 1 MiB | `TypeError` at the call that sends it: JSON crosses as doubles and UTF-8 |
| Leaves a file open across a wait                             | The run fails: "A file is open across a wait"                            |

CPython's guard against deep C recursion measures its stack in linear memory,
but every C frame also takes Wasmtime's native stack, which the guard cannot
see, at 10 to 30 times the rate. With Wasmtime's default 512 KiB, deep
recursion trapped long before the guard fired, whatever its budget. The
runtime now gives VMs 32 MiB of native stack (address space, touched only as
deep as a guest recurses) and the guard a budget of 512 KiB (`reactor.c`):
recursion 100,000 deep raises `RecursionError` in under half a second.

## The standard library

A durable run that imports every module of `sys.stdlib_module_names` imports 245. Of the 50 others:

- **Cannot work in WASI:** `ssl`, `ctypes`, `mmap`, `multiprocessing`,
  `subprocess`'s internals, `curses`, `readline`, `termios`, `pty`, `tty`,
  `pwd`, `grp`, `fcntl`, `resource`, `syslog`, subinterpreters, and the
  Windows and macOS modules.
- **Need a C library the build lacks:** `bz2`, `lzma`, `zstd` (compression)
  and `sqlite3` (programs have `ctx.db`).

`decimal` (C), `zlib` and `gzip`, `zoneinfo` (with `tzdata`), `sysconfig`,
`hashlib`'s common hashes, `json`, `re`, `asyncio`, `dataclasses`, `typing`
and `datetime` all work.

## What it costs

`cargo bench --bench languages` runs the same workloads in both languages:
medians of 15 runs, on one machine, against a store in memory.

| Metric                                                 | JavaScript | Python                       |
| ------------------------------------------------------ | ---------- | ---------------------------- |
| Golden image (memory)                                  | 1.4 MB     | 8.6 MB                       |
| Evaluating a program at startup                        | ~3 ms      | 18–24 ms (the first ~250 ms) |
| A call's first turn (`scalar`)                         | 1.0 ms     | 2.6 ms                       |
| A turn on a live VM (`scalar`)                         | 0.2–0.3 ms | 0.6–0.75 ms                  |
| A turn on a restored VM (`scalar`)                     | 1.0 ms     | 2.1–2.3 ms                   |
| Pages a turn changes (`scalar`)                        | 20–38      | 58–120                       |
| Pages a turn changes, 2,000 new objects (`heap-churn`) | 110–170    | 213–301                      |
| Batch a turn writes to Restate (`heap-churn`)          | 48–55 KB   | 56–61 KB                     |
| Primes below 100,000                                   | 264 ms     | 330 ms                       |

Native CPython 3.13 runs the primes loop in ~300 ms on the same machine:
CPython in Wasm is about as fast as natively. What a turn writes to Restate,
which dominates throughput ([ENGINES.md](ENGINES.md)), is about the same in
both languages.

Against a Restate server (1.8.0, on the same 4 vCPUs as the load and the
runtime; two rounds of equivalent programs in both languages):

| Workload                                      | JavaScript             | Python                 |
| --------------------------------------------- | ---------------------- | ---------------------- |
| KV read and write, a new key each, 32 at once | 289–342/s, p50 ~100 ms | 178–183/s, p50 ~168 ms |
| KV read and write, one at a time              | p50 14 ms              | p50 16 ms              |
| 5 waits, 32 runs at once                      | 170–175/s              | 99–102/s               |
| 20 waits on a 1 ms timer, one run             | 99–104 ms              | 122–125 ms             |

One at a time, a Python call is within ~15% of a JavaScript one; under load,
where the CPU is shared, Python serves ~55–60% as many, as its turns cost 2–3
times the CPU. The process stayed at ~400 MiB through both.

800 Python runs held open at once peak at ~3 GB, ~3.5 MB a run. The server
gives the freed memory back to the system within 5 s of such a burst
(`malloc_trim`, `src/main.rs`): it falls to ~150 MB, where glibc's allocator
alone kept 1.3–1.4 GB for reuse.

From the first build, these brought Python's figures down:

- **Bytecode for the standard library.** Without it, every program's
  evaluation compiled asyncio and its imports from source: about 1 s.
- **A cached compiled module.** Cranelift takes ~8 s to compile CPython. The
  runtime serializes the module into its cache directory and loads it from
  there afterwards, as it does QuickJS's (~1 s): a server restarts in ~0.35 s
  with both engines, where it took ~3 s compiling QuickJS twice, with and
  without its data segments, before each engine had one module.
- **`gc.freeze()` after load.** Pages changed per turn fell from 85–147 to
  66–109, and from 680–1,190 to 225–323 under heap churn; batches by half.
- **A shared base image.** CPython starts and imports asyncio once per
  process; a program's golden image is that base with the program loaded:
  ~110 ms became 18–24 ms.
- **Golden mapped copy-on-write** ([DESIGN.md](DESIGN.md) §3.2): starting and
  restoring a VM took 1.7 + 3.9 ms, writing the 11 MB image into it; mapped,
  30 + 26 µs. A restored turn went from 7.8–8.8 ms to 3.9–4.4 ms.
- **Walking the heap only where a turn wrote** (DESIGN.md §5.6): without
  pymalloc every object is a chunk, and the whole walk took 0.5–1.5 ms a
  turn. A restored turn went to 2.3 ms.
- **Only asyncio's core in the base** (above): the golden image went from
  11.4 to 8.6 MB and a server's start with the Python examples from 0.36 to
  0.31 s; a turn changes 5–20% fewer pages.

## Other engines

| Engine                                                                 | For us                                                                                                                                                                                                                                                                                                                                                                |
| ---------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CPython for WASI (this)                                                | All of Python 3.15 and its standard library; the largest image, which copy-on-write makes cheap to start                                                                                                                                                                                                                                                              |
| [Monty](https://github.com/pydantic/monty) (Pydantic)                  | A Python subset in Rust that pauses at host calls and serializes its own state (kilobytes, under 1 ms to start). No class inheritance, `yield`, `@property`, `async with`/`for`, `except*`; asyncio is `run`, `gather` and `sleep`. Built for short code a model writes, not for programs people write. A fit for a sandboxed "run this snippet" tool inside an agent |
| [MicroPython](https://github.com/micropython/micropython)              | A 1–2 MB image (not measured), MIT. Python 3.4 with later additions, a subset of asyncio, a small standard library; its WASI build is not merged upstream                                                                                                                                                                                                             |
| [RustPython](https://github.com/RustPython/RustPython)                 | Builds for WASI; no published footprint or speed; slower than CPython by reputation                                                                                                                                                                                                                                                                                   |
| [componentize-py](https://github.com/bytecodealliance/componentize-py) | CPython 3.14 as a component, pre-initialized like Wizer; several memories and globals: not our one-memory ABI                                                                                                                                                                                                                                                         |
| Pyodide                                                                | CPython for Emscripten: its system lives in JavaScript, outside the memory we save                                                                                                                                                                                                                                                                                    |

No Wasm runtime but Wasmtime maps an image copy-on-write into a pooled
instance (Wasmer copies; WasmEdge and WAMR have proposals). Wasmtime takes the
image only from a module's data segments ([#14347](https://github.com/bytecodealliance/wasmtime/issues/14347)
asks for more), which is why the runtime maps it itself.

## Next

- **pymalloc: tried, not taken.** It is off in CPython's WASI build, so every
  object is a dlmalloc chunk. On (4 KiB pools in 256 KiB arenas, taken from
  `malloc`, so the heap walk still works), a turn changed 10–20% fewer pages,
  but under heap churn its batches grew from 55–59 to 68–105 KB: freed blocks
  inside pools keep their bytes, which no walk knows to be dead. Patching
  `pymalloc_free` to zero each freed block made pages 12–32% fewer and most
  batches smaller, yet one heap-churn turn still grew (55 to 71 KB) and ran
  slower (6.5 to 9.6 ms). Not worth carrying a patch to CPython's allocator.
- **Packages.** A program can import the standard library only. Pure-Python
  packages could join the tree; packages with C extensions would need
  building for WASI and linking into the reactor.

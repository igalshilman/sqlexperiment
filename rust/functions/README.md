# Durable JavaScript and Python in Restate objects

Run a plain JavaScript or Python module as a Restate virtual object, its
functions the handlers. A call can wait as long
as it needs to: on a timer, a call to another service, or a model. It survives
crashes and restarts, and Restate never replays it. Each time the program
waits, the runtime saves its VM's memory. When a result arrives, or
after a crash, the runtime restores that memory and carries on from where the
program stopped.

```js
// examples-functions/counter.js: the virtual object `counter`, its exported
// functions the handlers
export async function add(n, ctx) {
  const count = ((await ctx.kv.get('count')) ?? 0) + n;
  await ctx.kv.set('count', count);
  await ctx.sleep(1000);
  return count;
}

export async function get(_, ctx) {
  return (await ctx.kv.get('count')) ?? 0;
}
```

```python
# examples-functions-python/counter.py: the same object, in Python; every
# function the module defines (but _private ones) is a handler
async def add(n, ctx):
    count = (await ctx.kv.get("count") or 0) + n
    await ctx.kv.set("count", count)
    await ctx.sleep(1)
    return count


async def get(_, ctx):
    return await ctx.kv.get("count") or 0
```

The functions this binary serves, how to run them and how to write one:
[examples-functions/](../../examples-functions/README.md), and in Python
[examples-functions-python/](../../examples-functions-python/README.md). The coding agent's
workspace is the [`workspace`](src/workspace.rs) object. Programs reach
models through the host's gateway, [`Llm`](src/llm.rs): any provider's, in
one format, with conversations kept in the scope's database
([docs/LLM.md](docs/LLM.md)); and the web with `fetch`, under a
[`FetchPolicy`](src/fetch.rs) that says where requests may go and which
secrets the host adds ([docs/FETCH.md](docs/FETCH.md)). Both are among the
deployment's [`Extensions`](src/extension.rs), what programs use beside their
memory and their database; `fetch` is the simplest kind, a
[`Binding`](src/bindings.rs): one operation the host makes for a program
([docs/HOST.md](docs/HOST.md) §6).

## How it works

QuickJS (or CPython, for Python: [docs/PYTHON.md](docs/PYTHON.md)) runs as
WebAssembly, so the whole VM (heap, closures, pending promises or tasks) is
one byte array. A run is one call of a handler, without journal
replay, looping over turns on one live VM:

```
deliver one result ─▶ run until the JavaScript waits ─▶ flush ─▶ wait on Restate ─┐
       ▲                                                                          │
       └──────────────────────────────────────────────────────────────────────────┘
```

- **Snapshot only at a wait**, when no frame is on the stack: memory is the
  whole state.
- **A live VM**: the VM stays in memory from turn to turn of an attempt; only
  a new attempt (after a crash) restores it, once.
- **Relative to a golden image**: each program, evaluated once at startup
  (deterministically, so every process agrees).
- **Only what changed**: the kernel reports which 4 KiB pages a turn wrote
  (`userfaultfd` write protection and `PAGEMAP_SCAN`, on Linux 6.7 or later;
  elsewhere, rewritten Wasm marks them); a turn writes those pages to the
  scope's memory file, a file over the golden image: a page equal to
  golden's is a hole, and a page is compressed against golden's the first
  time, then against its previous image. Dead bytes (the stack between
  calls, freed heap memory) are zeroed first, so garbage is not saved.
- **One flush**: a turn is one flush of the scope's
  [state](../../core/state), one put of its commit record: its memory pages,
  its SQL (`ctx.kv` is a table too) and the run's progress with its pending
  requests, written into one WAL. The state stores the values the record
  names before it puts the record, so a crash never leaves a record naming
  values that are not stored.
- **No replay**: a retry restores the VM from the last flush and waits
  again on its requests under the same idempotency keys, attaching to calls
  already made.

## What it costs

Against the same workloads as regular TypeScript virtual objects (SDK 1.17.3,
Node 22), one restate-server 1.7.13 on 4 vCPUs:

| Workload                                      | TypeScript       | QuickJS          |
| --------------------------------------------- | ---------------- | ---------------- |
| KV read and write, a new key each, 32 at once | 473/s, p50 62 ms | 362/s, p50 83 ms |
| KV read and write, one key, one at a time     | p50 10 ms        | p50 13 ms        |
| 20 waits on a 1 ms timer, one run             | 115 ms           | 95 ms            |
| 5 waits, 32 runs at once                      | 175/s            | 134/s            |
| `Promise.all` of 10 calls, 8 at once          | 38/s             | 28/s             |
| Primes below 100,000                          | 47 ms            | 519 ms           |
| Written to Restate by a run of 20 waits       | ~20 KiB          | ~53 KiB          |

Primes is the interpreter: QuickJS interprets, V8 compiles. The other rows
are mostly what each turn writes to Restate, not the JavaScript. Which parts
are QuickJS's, and which engines could replace it:
[docs/ENGINES.md](docs/ENGINES.md).

Details, formats and measurements: [docs/DESIGN.md](docs/DESIGN.md). Tests:
`make test-functions` (with a Restate server), `make lint-functions`.

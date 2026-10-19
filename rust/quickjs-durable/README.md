# Durable JavaScript in Restate objects

Run plain JavaScript as a Restate virtual object. A program can wait as long
as it needs to: on a timer, a call to another service, or a model. It survives
crashes and restarts, and Restate never replays it. Each time the program
waits, the runtime saves the JavaScript VM's memory. When a result arrives, or
after a crash, the runtime restores that memory and carries on from where the
program stopped.

```js
// examples-functions/counter.js: served as the virtual object `counter`
async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  await ctx.sleep(1000);
  return n;
}
```

The functions this binary serves, how to run them and how to write one:
[examples-functions/](../../examples-functions/README.md). The coding agent's
workspace is the [`workspace`](src/workspace.rs) object, and models are
[`claude`](src/claude.rs).

## How it works

QuickJS runs as WebAssembly, so the whole VM (heap, closures, pending
promises) is one byte array. A run is one invocation of `run`, without journal
replay, looping over turns on one live VM:

```
deliver one result ─▶ run until the JavaScript waits ─▶ commit ─▶ wait on Restate ─┐
       ▲                                                                           │
       └───────────────────────────────────────────────────────────────────────────┘
```

- **Snapshot only at a wait**, when no frame is on the stack: memory is the
  whole state.
- **A live VM**: the VM stays in memory from turn to turn of an attempt; only
  a new attempt (after a crash) restores it, once.
- **Relative to a golden image**: each program, evaluated once at startup
  (deterministically, so every process agrees).
- **Only what changed**: the kernel reports which 4 KiB pages a turn wrote
  (`userfaultfd` write protection and `PAGEMAP_SCAN`, on Linux 6.7 or later;
  elsewhere, rewritten Wasm marks them); a turn saves the 64-byte blocks of
  those pages that differ, compressed together against their contents before
  it, in a chain over a base relative to golden. Dead bytes (the
  stack between calls, freed heap memory) are zeroed first, so garbage is
  not saved.
- **One commit**: a turn is one put of the scope's
  [sqlite-kv](../../core/sqlite-kv) record: its page batch, its SQL (`ctx.kv`
  is a table too) and the run's state with its pending requests. Restate keeps
  writes in order, so whatever the record names is stored before it.
- **No replay**: a retry restores the VM from the last commit and waits
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
`make test-qjs` (with a Restate server), `make lint-qjs`.

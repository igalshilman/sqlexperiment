# Durable JavaScript in Restate objects

JavaScript that waits on Restate for as long as it takes, and survives crashes
without replaying anything: at each wait the VM's memory is saved, and a crash
or a result restores it.

```js
// programs/counter.js: served as the virtual object `counter`
async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  await ctx.sleep(1000);
  return n;
}
```

```sh
make restate-server qjs                               # from the repository's root
target/restate/restate-server &
ANTHROPIC_API_KEY=... target/release/quickjs-durable &
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/counter/alice/run --json 20       # 20, a second later
```

Each `programs/<name>.js` is the object `<name>`; its key is a scope, whose
runs share `ctx.kv`. More in [`programs/`](programs): a saga
([`checkout`](programs/checkout.js)), retries with backoff
([`retry`](programs/retry.js)), a deadline ([`timeout`](programs/timeout.js)),
and an AI agent with the shop as its tools ([`assistant`](programs/assistant.js)).

## Writing a program

`ctx.kv.get/set/delete(key)` for the scope's JSON state,
`ctx.rpc.call({ service, handler, key? }, input)` for any Restate service, and
`ctx.sleep(ms)`. Models are a service too: `claude/messages` takes a
[Messages API](https://platform.claude.com/docs/en/api/messages) request.

- All I/O goes through `ctx`; values are JSON.
- The run ends when `main` settles: unlike Node.js, promises still pending are
  dropped. Await what must happen.
- A scope runs one run at a time.
- A changed program is a new program: runs restore only with their source.

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
- **Only the pages that changed**: rewritten Wasm marks each written 4 KiB
  page; a turn saves those, compressed together against their contents
  before it, in a chain over a base relative to golden. Dead bytes (the
  stack between calls, freed heap memory) are zeroed first, so garbage is
  not saved.
- **One commit**: a turn puts its batch of pages, then one `commit` record
  (the image's chain of batches, the pending requests, the KV writes), then applies
  the KV writes. Restate loses only a suffix of writes on a crash, so the
  commit is the atomic step; the next attempt re-applies what follows it.
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

Details, formats and measurements: [docs/DESIGN.md](docs/DESIGN.md). Tests:
`make test-qjs` (with a Restate server), `make lint-qjs`.

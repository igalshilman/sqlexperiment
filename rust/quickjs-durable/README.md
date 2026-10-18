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
replay, looping over turns:

```
restore ─▶ deliver one result ─▶ run until the JavaScript waits ─▶ commit ─▶ wait on Restate ─┐
   ▲                                                                                          │
   └──────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Snapshot only at a wait**, when no frame is on the stack: memory is the
  whole state.
- **Relative to a golden image**: each program, evaluated once at startup
  (deterministically, so every process agrees).
- **Only the pages that changed**: rewritten Wasm marks each written 4 KiB
  page; a turn saves those, XORed against golden and compressed together.
- **One commit**: the pages, the KV writes and the new requests are one save
  of the object's state ([sqlite-kv](../../core/sqlite-kv/README.md)).
- **No replay**: a retry restores the last commit and waits again on its
  requests under the same idempotency keys, attaching to calls already made.

The cost: against a regular TypeScript object, a run that waits is ~6x slower
and writes ~50x more per wait. Details, formats and measurements:
[docs/DESIGN.md](docs/DESIGN.md). Tests: `make test-qjs` (with a Restate
server), `make lint-qjs`.

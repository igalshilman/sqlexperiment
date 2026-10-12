# SQL in Restate virtual objects

An experiment: each Restate virtual object gets its own SQLite-compatible
database, stored in the object's state, and its handlers run without journal
replay, so a retried attempt runs again against the state as stored.

Three layers, each on the one before:

| Path | What |
|---|---|
| [`sqlite-kv/`](sqlite-kv/README.md) | **The storage.** SQL over an asynchronous key/value store, for namespaces with one owner at a time: a Rust crate with its tests, benchmarks, [design](sqlite-kv/docs/DESIGN.md) and [TLA+ specification](sqlite-kv/spec/README.md), and its [Node.js addon](sqlite-kv/node/README.md). It knows nothing about Restate. |
| [`vendor/`](vendor/README.md) | **The SDK changes.** Patches to the Restate TypeScript SDK and its shared core: handlers without journal replay (`journalReplay: false`), their own context type with plain promises, state reads that are not journaled (service protocol V8), and `ctx.sql` on the addon. |
| [`examples/`](examples/README.md) | **The examples**, on the patched SDK: [SQL in virtual objects](examples/node/README.md) (how it works, a ledger and a shop), [a durable pi agent](examples/pi/README.md), [a LangGraph agent](examples/langgraph/README.md), and the crash and concurrency tests against a real Restate server. |

## Build and test

```sh
make              # build everything: the crate, the addon, the patched SDK, the examples
make test         # every test that needs no Restate server
make crash-test         # the examples through crashes, against a Restate server
make concurrency-test   # the context without journal replay, against a Restate server
make help
```

Besides Rust, the Node.js parts need node 20 or later; the patched TypeScript
SDK also needs pnpm, the `wasm32-unknown-unknown` target and `wasm-bindgen`
0.2.121 (`cargo install wasm-bindgen-cli --version 0.2.121`). The tests against
a Restate server need one with service protocol V8, which the Makefile takes
out of a Restate image with docker (see [the examples](examples/README.md#validation)).

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

## Requirements

The SQL handlers need the **latest, unreleased Restate**: their state reads
are not journaled, which takes service protocol V8. No released Restate speaks
it, and neither does `restate:main` yet:

| | Version |
|---|---|
| Restate server | 1.8.0-rc.2 built from restate's `service-protocol-v8` branch: the `ghcr.io/restatedev/restate:service-protocol-v8` image, started with `RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true` (V8 is off by default) |
| TypeScript SDK | 1.17.3 with [the patches](vendor/README.md), over the shared core's unreleased main (the last release, 7.0.3, has no V8); `make` builds it |

With an older server, or without the flag, the server and the SDK settle on
V7, and every state read fails its attempt ("Feature 'ephemeral state get' is
not supported by the negotiated protocol version"), as Restate's retry policy
retries it.

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway -e RESTATE_EXPERIMENTAL_ENABLE_PROTOCOL_V8=true ghcr.io/restatedev/restate:service-protocol-v8
```

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

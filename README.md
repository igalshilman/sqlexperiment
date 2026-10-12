# SQL in Restate virtual objects

An experiment: each Restate virtual object gets its own SQLite-compatible
database, stored in the object's state, and its handlers run without journal
replay, so a retried attempt runs again against the state as stored.

Three layers, each on the one before:

| Path | What |
|---|---|
| [`sqlite-kv/`](sqlite-kv/README.md) | **The storage.** SQL over an asynchronous key/value store, for namespaces with one owner at a time: a Rust crate with its tests, benchmarks, [design](sqlite-kv/docs/DESIGN.md) and [TLA+ specification](sqlite-kv/spec/README.md), and its [Node.js addon](sqlite-kv/node/README.md). It knows nothing about Restate. |
| [`vendor/`](vendor/README.md) | **The SDK changes.** Patches to the Restate TypeScript SDK and its shared core: handlers without journal replay (`journalReplay: false`), their own context type with plain promises, and `ctx.sql` on the addon. |
| [`examples/`](examples/README.md) | **The examples**, on the patched SDK: [SQL in virtual objects](examples/node/README.md) (how it works, a ledger and a shop), [a durable pi agent](examples/pi/README.md), [a LangGraph agent](examples/langgraph/README.md), and the crash and concurrency tests against a real Restate server. |

## Requirements

| | Version |
|---|---|
| Restate server | 1.7.13, the latest release (or later) |
| TypeScript SDK | 1.17.3 with [the patches](vendor/README.md), over the shared core's unreleased main; `make` builds it |

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13
```

Or run `restate-server` directly: `make restate-server` downloads the release
binary for this platform to `target/restate/restate-server`.

## Build and test

With [Nix](https://nixos.org) (flakes), `nix develop` gives a shell with every
tool below, on macOS and Linux ([`flake.nix`](flake.nix)):

```sh
nix develop       # or `direnv allow`, with use flake in .envrc
make              # build everything: the crate, the addon, the patched SDK, the examples
make test         # every test that needs no Restate server
make crash-test         # the examples through crashes, against a Restate server
make concurrency-test   # the context without journal replay, against a Restate server
make help
```

Without Nix: besides Rust, the Node.js parts need node 22 or later; the
patched TypeScript SDK also needs pnpm, the `wasm32-unknown-unknown` target and
`wasm-bindgen` 0.2.121 (`cargo install wasm-bindgen-cli --version 0.2.121`).

The tests against a Restate server download its release binary on first use
(see [the examples](examples/README.md#validation)).

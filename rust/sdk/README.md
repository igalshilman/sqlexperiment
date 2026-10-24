# The patched Rust SDK

The Restate Rust SDK with handlers that do not replay their journal, over the
released [shared core 8.1.0](https://github.com/restatedev/sdk-shared-core/releases/tag/v8.1.0) (`JournalReplay::Disabled`). It is what
[the durable JavaScript objects](../functions/README.md) run on.

|          | What                                             | Version                                                                                                                                     | Form                                            |
| -------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| [`.`](.) | `restate-sdk` (Rust) and the shared core it uses | 0.12.1 ([`5cd9a3d`](https://github.com/restatedev/sdk-rust/commit/5cd9a3d24d7ddc21f29c8c43074d3d67ca67c163)), shared core 8.1.0 (crates.io) | Patches, applied to a fresh clone by `setup.sh` |

```rust
let options = ServiceOptions::new()
    .journal_replay(false)
    .enable_lazy_state(true)
    .eager_state_keys(["v16/root"]);
let endpoint = Endpoint::builder().bind(Counter.options(options)).build();
```

A handler without journal replay runs again from the start on a retried
attempt, against the state as stored: its earlier commands are skipped, never
matched, so it may take a different path. Calls and sends are made again,
unless their idempotency key is the same on every attempt; `ctx.run` runs
again; awakeables are refused (the core cannot make their ids fresh).

Futures may wait together, such as several state reads at once: the one that
reads the runtime's next message wakes the others, whose results it may
carry. (Without the patch, only the last future to wait is woken, and the
others hang.)

> **Upstream bug, patched here only.** This one is a bug in the released SDK
> (0.12.1), not a change for SQL: any handler that awaits two futures at once
> can hang (`futures::join!` on two state reads, say). We keep the fix in this
> patch and have not proposed it to `restatedev/sdk-rust`; drop it once the
> SDK fixes it on its own.

The HTTP server sets `TCP_NODELAY` on each connection. Without it, each
small message to Restate waits for the peer's delayed acknowledgement, about
40 ms: a handler that waits on Restate many times (a state read, a call, a
timer) loses that much at each. Also an upstream bug, patched here only.

`ObjectContext::raw_state()` is the object's state as bytes, through a handle
that outlives the context's borrow, for a store that must own its storage,
such as [a database in the state](../../core/sql/README.md).

## Build

[`setup.sh`](setup.sh) (`make sdk-rs`) clones the SDK at the pinned revision
into `.build/` (ignored), and applies [`patches/`](patches). After editing the
clone, [`patches/regenerate.sh`](patches/regenerate.sh) writes the patch back;
`setup.sh` refuses to clone again over edits that the patch does not have. `make test-sdk-rs` runs the SDK's unit tests.

## Changes

| Change                                                                                                                                                                                                   | Why                                                                                                                                 | Where                                                                                                                                      |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| The released shared core 8.1.0, whose `JournalReplay::Disabled` runs a retried attempt again from the start; state commands named after their key                                                        | So that tooling can hide a SQL database's pages and log by name (`dev.restate.tooling.journal.hidden.name.prefix`)                  | `Cargo.toml`, `endpoint/context.rs`                                                                                                        |
| `ServiceOptions::journal_replay` and `HandlerOptions::journal_replay`, passed to the VM's constructor                                                                                                    | Turns replay off per service, or per handler; an SDK-side option, not in the discovery                                              | `endpoint/builder.rs`, `endpoint/mod.rs`, `service.rs`                                                                                     |
| `ObjectContext::raw_state()` and `RawState`: `get`, `set` and `clear` on bytes, and `sync` (an empty `ctx.run`)                                                                                          | A store that owns its storage (`'static`), such as journal's ordered `KvStore`                                                      | `context/mod.rs`, `lib.rs`                                                                                                                 |
| `ServiceOptions::name`                                                                                                                                                                                   | Bind one type as several services, each with state of its own: one object per program                                               | `endpoint/builder.rs`                                                                                                                      |
| Futures waiting for input wake each other once one has read it, and check their result before reading more                                                                                               | Concurrent awaits, such as journal's concurrent reads: the input channel wakes only the last waiter                                 | `endpoint/context.rs`, `endpoint/futures/async_result_poll.rs`, `select_poll.rs`                                                           |
| `TCP_NODELAY` on each connection of the HTTP server                                                                                                                                                      | Nagle's algorithm delays each small message by the peer's delayed ACK (~40 ms)                                                      | `http_server.rs`                                                                                                                           |
| `RawState::get` is protocol V8's unjournaled get (`get_ephemeral`, `EphemeralGetFuture`) where V8 is negotiated, a journaled get on V7 (TODO, [FUTURE.md](../../FUTURE.md)); the discovery advertises V8 | Unjournaled reads add nothing to the journal: on replay they would read the state again anyway. Restate 1.8 negotiates V7 at most   | `context/mod.rs`, `endpoint/context.rs`, `endpoint/futures/ephemeral_poll.rs`, `async_result_poll.rs`, `select_poll.rs`, `endpoint/mod.rs` |
| `ServiceOptions::eager_state_keys` and `HandlerOptions::eager_state_keys`: the discovery's `eagerStateKeysWhitelist` (Restate 1.8.0 and later), added to `endpoint_manifest_schema.json`                 | With lazy state, the keys Restate still sends with each invocation: the state's root, read first by every turn, costs no round trip | `endpoint/builder.rs`, `discovery.rs`, `endpoint_manifest_schema.json`                                                                     |
| Build: no `rust-toolchain.toml`                                                                                                                                                                          | The toolchain is the repository's (`flake.nix`), not a second pinned one                                                            | `rust-toolchain.toml`                                                                                                                      |

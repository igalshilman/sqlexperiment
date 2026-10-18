# The patched Rust SDK

The Rust counterpart of [the patched Python SDK](../../python/sdk/README.md):
the Restate Rust SDK with handlers that do not replay their journal, over
[the patched shared core](../../core/sdk-shared-core/README.md). It is what
[the durable JavaScript objects](../quickjs-durable/README.md) run on.

|          | What                                             | Version                                                                                                                                                                                                                              | Form                                           |
| -------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------- |
| [`.`](.) | `restate-sdk` (Rust) and the shared core it uses | 0.12.1 ([`5cd9a3d`](https://github.com/restatedev/sdk-rust/commit/5cd9a3d24d7ddc21f29c8c43074d3d67ca67c163)), shared core [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45) | Patches, applied to fresh clones by `setup.sh` |

```rust
let endpoint = Endpoint::builder()
    .bind(Counter.options(ServiceOptions::new().journal_replay(false).enable_lazy_state(true)))
    .build();
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

`ObjectContext::raw_state()` is the object's state as bytes, through a handle
that outlives the context's borrow, for a store that must own its storage,
such as [a database in the state](../../core/sqlite-kv/README.md).

## Build

[`setup.sh`](setup.sh) (`make sdk-rs`) clones both repositories at the pinned
revisions into `.build/` (ignored), and applies [`patches/`](patches) and
[the shared core's patch](../../core/sdk-shared-core/sdk-shared-core.patch).
The SDK's `Cargo.toml` takes the core from the clone next to it, and so must a
crate that depends on the SDK (a `[patch.crates-io]` of its own). After
editing the clones, [`patches/regenerate.sh`](patches/regenerate.sh) writes
the patches back; `setup.sh` refuses to clone again over edits that the
patches do not have. `make test-sdk-rs` runs the SDK's unit tests.

## Changes

| Change                                                                                                          | Why                                                                                                   | Where                                                                            |
| --------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `JournalReplay::Disabled`, and state commands named after their key                                             | See [the shared core](../../core/sdk-shared-core/README.md)                                           | `sdk-shared-core.patch`                                                          |
| `ServiceOptions::journal_replay` and `HandlerOptions::journal_replay`, passed to the VM's constructor           | Turns replay off per service, or per handler; an SDK-side option, not in the discovery                | `endpoint/builder.rs`, `endpoint/mod.rs`, `service.rs`                           |
| `ObjectContext::raw_state()` and `RawState`: `get`, `set` and `clear` on bytes, and `sync` (an empty `ctx.run`) | A store that owns its storage (`'static`), such as sqlite-kv's ordered `KvStore`                      | `context/mod.rs`, `lib.rs`                                                       |
| `ServiceOptions::name`                                                                                          | Bind one type as several services, each with state of its own: one object per program                 | `endpoint/builder.rs`                                                            |
| Futures waiting for input wake each other once one has read it, and check their result before reading more      | Concurrent awaits, such as sqlite-kv's concurrent reads: the input channel wakes only the last waiter | `endpoint/context.rs`, `endpoint/futures/async_result_poll.rs`, `select_poll.rs` |
| `AwaitResponse::EphemeralNotificationReady` is unexpected                                                       | The core's protocol V8 has ephemeral commands, which this SDK does not make                           | `endpoint/futures/select_poll.rs`, `async_result_poll.rs`                        |
| Build: the core from the clone next to the SDK (`[patch.crates-io]`); no `rust-toolchain.toml`                  | The patched core; the toolchain is the repository's (`flake.nix`), not a second pinned one            | `Cargo.toml`, `rust-toolchain.toml`                                              |

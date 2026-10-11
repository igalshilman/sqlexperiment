# Vendored and patched code

The Restate integrations need SDK changes that are not released: an
invocation that does not replay its journal, and a few fixes. This directory
holds those SDKs and the changes.

| | What | Version | Form |
|---|---|---|---|
| [`rust/`](rust) | `restate-sdk`, `restate-sdk-macros`, `restate-sdk-shared-core` | 0.12.1, 0.12.1, 7.0.3 (from crates.io) | Full copies, patched in place |
| [`typescript/`](typescript) | `@restatedev/restate-sdk` and the shared core it compiles to WebAssembly | 1.17.3 ([`02a12acb`](https://github.com/restatedev/sdk-typescript/commit/02a12acb879fcf46448d88d028304d3a3113e7e2)), shared core [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45) | Patches, applied to fresh clones by `setup.sh` |

## Rust

Each crate was first committed unmodified (under `restate/vendor`, before it
moved here), so `git log -p -- restate/vendor vendor/rust` shows the
changes. The crates that use them
([`restate/`](../restate) and [`examples/rust/ledger`](../examples/rust/ledger))
point crates.io's names at these copies with `[patch.crates-io]`.

| Change | Why | Where |
|---|---|---|
| `JournalReplay::Disabled`: after the input, skip the recorded commands; start completion ids at `1 + 2 × known entries` (a command takes at most two, so ids of earlier attempts are never reused); refuse awakeables, whose signal ids cannot be made fresh | A retried attempt runs the handler again against the state as stored, instead of replaying a journal its reads would not match | shared core `VMOptions`, `SysInput`, `Journal::skip_replayed` |
| `ServiceOptions::journal_replay(bool)`, recorded in the service metadata and applied to each invocation's VM | Turns the above on per service | SDK `endpoint/builder.rs`, `endpoint/mod.rs` |
| `ObjectContext::internal()` | A store holds the context beyond a borrow | SDK `context/mod.rs` |
| Handlers may take a custom context whose type name ends in `ObjectContext` (or another context's name) | `SqlObjectContext` | macros `struct_ast.rs` |
| **Fix:** futures awaited concurrently in one task lost wake-ups and hung until the inactivity timeout | A bug in the published SDK: the input stream woke only the last future to poll it | SDK `endpoint/futures/async_result_poll.rs` |
| **Fix:** set `TCP_NODELAY` on accepted connections | A bug in the published SDK: Nagle's algorithm delayed every `ctx.run` by about 42 ms | SDK `http_server.rs` |

The two fixes are bugs in the SDK as published, not specific to this
experiment.

**Concurrent awaits.** The input stream wakes only the last future to poll it. Once
another future had read the input that completed a handle, the future waiting
on that handle was never woken, and then went back to reading input rather
than checking for its result. Session I/O awaits several state reads at once
(prefetch and readahead), so invocations hung until Restate's
60-second inactivity timeout. Now every future waiting for input is woken
when input arrives, and a woken future checks for its result first.

**`TCP_NODELAY`.** `ctx.run` sends its command and then its result as two
small writes. With Nagle's algorithm the second waits for the server's delayed
acknowledgement: every `ctx.run` took about 42 ms longer. A cached transfer
took 62 ms with the barrier, and takes 15.7 ms with the fix.

## TypeScript

[`typescript/setup.sh`](typescript/setup.sh) clones both repositories at the
pinned revisions into `typescript/.build/` (ignored), applies
[`typescript/patches/`](typescript/patches), builds the shared core to
WebAssembly, inlines it into the SDK, and packs the SDK as
`typescript/.build/restatedev-restate-sdk-1.17.3.tgz`, which
[`examples/node`](../examples/node) installs. It needs cargo with the
`wasm32-unknown-unknown` target, `wasm-bindgen` 0.2.121, node and pnpm. After
editing the clones, [`typescript/patches/regenerate.sh`](typescript/patches/regenerate.sh)
writes the patches back.

| Change | Why | Where |
|---|---|---|
| `JournalReplay::Disabled`, as in Rust | As in Rust | `sdk-shared-core.patch` |
| A `journalReplay` service and handler option, passed to the WebAssembly VM's new constructor argument | Turns it on per service | `types/rpc.ts`, `endpoint/components.ts`, `endpoint/handlers/generic.ts`, the bindings' `lib.rs` |
| `ObjectContext.sql(body)` and `ObjectContext.sqlSession(body)`, `restate.sqlOptions`, the `SqlDatabase` and `SqlSession` types | SQL in a virtual object: the database over the object's state, through the sqlite-kv addon (an optional peer dependency, loaded on first use) | `sql.ts`, `context.ts`, `context_impl.ts` |
| Without journal replay, a call or send without an idempotency key gets one derived from the invocation id and the request | A retried attempt reaches the invocation its earlier attempt started, and gets its result, instead of starting another | `idempotency.ts`, `context_impl.ts` |
| `restate.internal.setSqlStoreWrapper` | Fault injection in the crash tests | `sql.ts`, `internal.ts` |
| **Fix:** several promises may be awaited at once | A limitation of the published SDK: a second concurrent await failed with "awaitNext already pending". The progress channel now wakes every waiter, and a waiter that saw generation *n* returns at once if a signal came after *n*, so no wake-up is lost | `utils/external_progress_channel.ts`, `promises.ts` |
| Build: the bindings depend on the patched core by path; [`inline-wasm.mjs`](typescript/inline-wasm.mjs) inlines the WebAssembly; the sqlite-kv peer dependency is added before packing | The SDK's own inliner (wasm-pack-inline 0.1.4) slices wasm-bindgen's output at positions that current wasm-bindgen no longer produces | `sdk-typescript.patch`, `setup.sh` |

The shared core's tests pass with the patch (280), and so do the TypeScript
SDK's (38).

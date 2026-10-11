# Vendored and patched code

The Restate integration needs SDK changes that are not released: an
invocation that does not replay its journal, and a few fixes. This directory
holds those changes.

| | What | Version | Form |
|---|---|---|---|
| [`typescript/`](typescript) | `@restatedev/restate-sdk` and the shared core it compiles to WebAssembly | 1.17.3 ([`02a12acb`](https://github.com/restatedev/sdk-typescript/commit/02a12acb879fcf46448d88d028304d3a3113e7e2)), shared core [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45) | Patches, applied to fresh clones by `setup.sh` |
| [`rust/restate-sdk-shared-core`](rust/restate-sdk-shared-core) | The shared core alone | 7.0.3 (from crates.io) | A full copy, patched in place |

## The shared core

`JournalReplay::Disabled`: after the input, the VM skips the recorded
commands; completion ids start at `1 + 2 × known entries` (a command takes at
most two, so ids of earlier attempts are never reused); awakeables, whose
signal ids cannot be made fresh, are refused. A retried attempt then runs the
handler again against the state as stored, instead of replaying a journal its
reads would not match. The change is in `VMOptions`, `SysInput` and
`Journal::skip_replayed`.

The TypeScript build applies it to the shared core's repository with
[`typescript/patches/sdk-shared-core.patch`](typescript/patches/sdk-shared-core.patch).
[`rust/restate-sdk-shared-core`](rust/restate-sdk-shared-core) is the same
change on the published crate, from an earlier Rust integration (since
removed); nothing builds it now. It was first committed unmodified, so
`git log -p -- restate/vendor/restate-sdk-shared-core vendor/rust` shows the
change.

## TypeScript

[`typescript/setup.sh`](typescript/setup.sh) clones both repositories at the
pinned revisions into `typescript/.build/` (ignored), applies
[`typescript/patches/`](typescript/patches), builds the shared core to
WebAssembly, inlines it into the SDK, and packs the SDK as
`typescript/.build/restatedev-restate-sdk-1.17.3.tgz`, which
[`examples`](../examples) installs. It needs cargo with the
`wasm32-unknown-unknown` target, `wasm-bindgen` 0.2.121, node and pnpm. After
editing the clones, [`typescript/patches/regenerate.sh`](typescript/patches/regenerate.sh)
writes the patches back.

| Change | Why | Where |
|---|---|---|
| `JournalReplay::Disabled` | See [the shared core](#the-shared-core) | `sdk-shared-core.patch` |
| A `journalReplay` service and handler option, passed to the WebAssembly VM's new constructor argument | Turns it on per service | `types/rpc.ts`, `endpoint/components.ts`, `endpoint/handlers/generic.ts`, the bindings' `lib.rs` |
| `ObjectContext.sql(body)` and `ObjectContext.sqlSession(body)`, `restate.sqlOptions`, the `SqlDatabase` and `SqlSession` types | SQL in a virtual object: the database over the object's state, through the sqlite-kv addon (an optional peer dependency, loaded on first use) | `sql.ts`, `context.ts`, `context_impl.ts` |
| Without journal replay, a call or send without an idempotency key gets one derived from the invocation id and the request | A retried attempt reaches the invocation its earlier attempt started, and gets its result, instead of starting another | `idempotency.ts`, `context_impl.ts` |
| `restate.internal.setSqlStoreWrapper` | Fault injection in the crash tests | `sql.ts`, `internal.ts` |
| Without journal replay, the handler's context has plain promises, driven by one loop | Concurrency is safe once nothing is replayed, but the protocol has one await point per invocation: a suspension, the awaiting-on hint and a cancellation each take one future. So each operation's promise is registered, when it is made, with a `PromiseSelector`, which asks the VM about all pending promises at once (one `FirstCompleted` future) and is the only one waiting for external progress, as the published SDK expects; user code gets a plain `Promise` to await and combine as it likes. See [the context](../examples/node/README.md#the-context-without-journal-replay) | `promise_selector.ts`, `unjournaled.ts`, `promises.ts`, `context_impl.ts`, `endpoint/handlers/generic.ts`, `test/promise_selector.test.ts` |
| Build: the bindings depend on the patched core by path; [`inline-wasm.mjs`](typescript/inline-wasm.mjs) inlines the WebAssembly; the sqlite-kv peer dependency is added before packing | The SDK's own inliner (wasm-pack-inline 0.1.4) slices wasm-bindgen's output at positions that current wasm-bindgen no longer produces | `sdk-typescript.patch`, `setup.sh` |

The shared core's tests pass with the patch (280), and so do the TypeScript
SDK's (57, of which 19 are the selector's, against a fake VM and the
published single-waiter progress channel).

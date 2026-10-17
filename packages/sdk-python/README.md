# The patched Python SDK

The Python counterpart of [the patched TypeScript SDK](../sdk-typescript/README.md):
the Restate Python SDK with an invocation that does not replay its journal,
over [the patched shared core](../sdk-shared-core/README.md), which it links
natively (PyO3) where the TypeScript SDK compiles it to WebAssembly.

|                             | What                                                     | Version                                                                                                                                                                                                                                | Form                                           |
| --------------------------- | -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| [`packages/sdk-python/`](.) | `restate-sdk` (Python) and the shared core it links with | 1.0.5 ([`ad4ea7c0`](https://github.com/restatedev/sdk-python/commit/ad4ea7c0a60b5745681a809fe57656bec0939c38)), shared core [`382649d`](https://github.com/restatedev/sdk-shared-core/commit/382649d0652aa6d971ec16e788d24b4b2c8c8b45) | Patches, applied to fresh clones by `setup.sh` |

## Build

[`setup.sh`](setup.sh) (`make sdk-py`) clones both repositories at the
pinned revisions into `.build/` (ignored), and applies
[`patches/`](patches) and [the shared core's patch](../sdk-shared-core/sdk-shared-core.patch).
The SDK's `Cargo.toml` takes the core from the clone next to it. The uv
workspace ([`pyproject.toml`](../../pyproject.toml)) installs the SDK from
`.build/sdk-python` wherever `restate-sdk` is used, building it with maturin
(`make py`), and again when its Rust or the core's changes. It needs git,
cargo, Python 3.13 and uv. After editing the clones,
[`patches/regenerate.sh`](patches/regenerate.sh) writes the patches back.

`make test-sdk-py` runs the SDK's own tests, in its own environment, except
those against a Restate server in docker (its test harness).

## Changes

| Change                                                                                                              | Why                                                                                                                                 | Where                                                                                  |
| ------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `JournalReplay::Disabled`, and state commands named after their key                                                 | See [the shared core](../sdk-shared-core/README.md)                                                                                 | `sdk-shared-core.patch`                                                                |
| A `journal_replay` option of services, objects and workflows, and of their handlers, passed to the VM's constructor | Turns it off per service, or per handler: a retried attempt then runs the handler again from the start, against the state as stored | `service.py`, `object.py`, `workflow.py`, `handler.py`, `server.py`, `vm.py`, `lib.rs` |
| `AwaitResponse::EphemeralNotificationReady` is an internal error                                                    | The core's protocol V8 has ephemeral commands, which this SDK does not make                                                         | `lib.rs`                                                                               |
| Build: the core from the clone next to the SDK (`[patch.crates-io]`); uv `cache-keys` over the Rust                 | The patched core; and uv rebuilds the extension when the Rust changes                                                               | `Cargo.toml`, `pyproject.toml`                                                         |

The SDK's tests pass with the patch (18, of which 3 are the option's, in
`tests/journal_replay.py`).

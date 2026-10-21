# The patched Python SDK

The Restate Python SDK with explicit recovery: a `journal_replay` option
that, off, has a retried attempt run the handler again from the start instead
of replaying its journal, over the released
[shared core 8.1.0](https://github.com/restatedev/sdk-shared-core/releases/tag/v8.1.0)
(`JournalReplay::Disabled`), which it links natively (PyO3). It is what the
TypeScript SDK 1.17.4 releases (`restate.internal.explicitRecovery()`), and
no more: SQL in virtual objects is a library over it,
[`restate-sql`](../sql/README.md), as in TypeScript. The patch goes once a
Python SDK release has explicit recovery ([FUTURE.md](../../FUTURE.md)).

|                    | What                                                     | Version                                                                                                                                       | Form                                            |
| ------------------ | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| [`python/sdk/`](.) | `restate-sdk` (Python) and the shared core it links with | 1.0.5 ([`ad4ea7c0`](https://github.com/restatedev/sdk-python/commit/ad4ea7c0a60b5745681a809fe57656bec0939c38)), shared core 8.1.0 (crates.io) | Patches, applied to a fresh clone by `setup.sh` |

## Build

[`setup.sh`](setup.sh) (`make sdk-py`) clones the SDK at the pinned revision
into `.build/` (ignored), and applies [`patches/`](patches). The uv
workspace ([`python/pyproject.toml`](../pyproject.toml)) installs the SDK from
`.build/sdk-python` wherever `restate-sdk` is used, building it with maturin
(`make py`), and again when its Rust changes. It needs git,
cargo, Python 3.13 and uv. After editing the clone,
[`patches/regenerate.sh`](patches/regenerate.sh) writes the patch back;
`setup.sh` refuses to clone again over edits that the patch does not have.

`make test-sdk-py` runs the SDK's own tests, in its own environment, except
those against a Restate server in docker (its test harness).

## Changes

| Change                                                                                                                                                                                                                                                    | Why                                                                                                                                 | Where                                                                                  |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| The released shared core 8.1.0, whose `JournalReplay::Disabled` runs a retried attempt again from the start; state commands named after their key                                                                                                         | So that tooling can hide a SQL database's pages and log by name (`dev.restate.tooling.journal.hidden.name.prefix`)                  | `Cargo.toml`, `lib.rs`                                                                 |
| A `journal_replay` option of services, objects and workflows, and of their handlers, passed to the VM's constructor                                                                                                                                       | Turns it off per service, or per handler: a retried attempt then runs the handler again from the start, against the state as stored | `service.py`, `object.py`, `workflow.py`, `handler.py`, `server.py`, `vm.py`, `lib.rs` |
| Protocol V8's ephemeral state get (`ephemeral_state_get`, `take_ephemeral_notification`, and its progress response); the discovery advertises V8. `restate-sql` reads with journaled gets until a Restate release ships V8 ([FUTURE.md](../../FUTURE.md)) | An ephemeral read adds nothing to the journal: without replay, nothing reads it back. Restate 1.8 negotiates V7 at most             | `lib.rs`, `vm.py`, `discovery.py`                                                      |
| Build: uv `cache-keys` over the Rust                                                                                                                                                                                                                      | uv rebuilds the extension when the Rust changes                                                                                     | `pyproject.toml`                                                                       |

The SDK's tests pass with the patch (18), with 3 of the option
(`tests/journal_replay.py`).

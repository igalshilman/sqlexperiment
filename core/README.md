# Core

What every SDK here builds on: the storage, and the change to the Restate
SDKs' shared core that runs a handler without journal replay.

| Path                                            | What                                                                                                                                                                                                                                                |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`sqlite-kv/`](sqlite-kv/README.md)             | **The storage.** SQL over an asynchronous key/value store, for namespaces with one owner at a time: a Rust crate. The [Node.js addon](../typescript/packages/sql-node/README.md) and the [Python extension](../python/sqlite-kv/README.md) wrap it. |
| [`sdk-shared-core/`](sdk-shared-core/README.md) | **The patched shared core**: the SDKs' VM, with `JournalReplay::Disabled`, as a patch that each SDK's build applies to a fresh clone.                                                                                                               |

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make core        # the crate
make test-core   # its tests, with the simulation campaigns
make lint-core   # rustfmt and clippy, for the crate, the addon and the Python extension
```

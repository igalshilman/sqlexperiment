# Core

What every SDK here builds on: the storage. (The Restate SDKs' shared core,
which runs a handler without journal replay since 8.1.0, is used as released.)

| Path                            | What                                                                                                                                                                                                                |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`journal/`](journal/README.md) | **The journal.** Records and paged files over an asynchronous key/value store, committed together in one put, for namespaces with one owner at a time: a Rust crate.                                                |
| [`sql/`](sql/README.md)         | **SQL on the journal**: SQLite-compatible SQL in the journal's file `db`, committed with it. The [Node.js addon](../typescript/packages/sql/README.md) and the [Python extension](../python/sql/README.md) wrap it. |

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make core        # the crates
make test-core   # their tests, with the simulation campaigns
make lint-core   # rustfmt and clippy, for the crates, the addon and the Python extension
```

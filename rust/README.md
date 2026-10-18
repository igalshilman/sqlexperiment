# Rust

The Rust side: the patched Restate Rust SDK, and what is built on it.

| Path                                            | What                                                                                                                                                                              |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`sdk/`](sdk/README.md)                         | **The SDK changes.** Patches to the Restate Rust SDK, over the patched shared core: handlers without journal replay, raw state, concurrent awaits                                 |
| [`quickjs-restate/`](quickjs-restate/README.md) | **Durable JavaScript in Restate objects**: a binary serving the programs in its `programs/`, each a virtual object of its name; its build evaluates them into their golden images |

```sh
make sdk-rs test-sdk-rs              # the patched SDK, and its unit tests
make qjs-restate test-qjs-restate    # the binary, and its tests against a Restate server
make lint-qjs-restate
```

# Rust

The Rust side: the patched Restate Rust SDK, and what is built on it.

| Path                                            | What                                                                                                                                                                                                 |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`sdk/`](sdk/README.md)                         | **The SDK changes.** Patches to the Restate Rust SDK, over the patched shared core: handlers without journal replay, raw state, concurrent awaits, `TCP_NODELAY`                                     |
| [`quickjs-durable/`](quickjs-durable/README.md) | **Durable JavaScript in Restate objects**: QuickJS in WebAssembly, its memory saved at each wait; a binary serving the durable functions of [`examples-functions/`](../examples-functions/README.md) |

```sh
make sdk-rs test-sdk-rs              # the patched SDK, and its unit tests
make qjs test-qjs                    # the binary, and its tests (the programs against a Restate server)
make lint-qjs
```

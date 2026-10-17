# QuickJS durable

Durable JavaScript without journal replay: QuickJS compiled to WebAssembly,
run by Wasmtime. At each durable wait, the runtime saves the VM's linear
memory as the 4 KiB pages that changed. Each page is XORed against a golden
image, and the changed pages are compressed together with Zstd. Recovery
restores that image, and the application's pending promises with it. It never
runs JavaScript again to rebuild state.

```js
async function main(input, ctx) {
  const n = await ctx.kv.get('count');
  await ctx.kv.set('count', n + 1);
  const answer = await ctx.rpc.call({ service: 'Worker', handler: 'double' }, n + 1);
  await ctx.kv.set('lastAnswer', answer);
  return answer;
}
```

The host owns each turn's transaction: the VM checkpoint, the application's
KV writes, the new effects and the consumed result commit together, or not at
all. Two hosts implement it: one on a SQLite file (`--features sqlite-host`),
and the same tables on [sqlite-kv](../sqlite-kv/README.md), in one namespace of
a key/value store such as a Restate object's state (`--features
sqlite-kv-host`). There is no binding to Restate's SDKs yet.

An experiment, imported from its own repository: see
[the design](docs/DESIGN.md), the complete specification.

## Build and test

From the repository's root:

```sh
make qjs        # the crate, with its SQLite host
make test-qjs   # its tests: with dirty-page tracking (verified), then with full scans
make lint-qjs   # rustfmt and clippy, in both capture modes
```

Or in this directory, a run of the SQLite example: run it again with the same
path and it recovers the saved result instead of incrementing again.

```sh
cargo run --release --features sqlite-host --example sqlite -- /tmp/quickjs.sqlite
```

| Path                               | What                                                                        |
| ---------------------------------- | --------------------------------------------------------------------------- |
| `src/`                             | The engine, the transaction interfaces, the VM and the Wasm instrumentation |
| `assets/`                          | The pinned QuickJS reactor (`quickjs.wasm`), the guest bridge and licenses  |
| `tests/`                           | Recovery, transactions, tracking, and crashes (SIGKILL) around commits      |
| `examples/`, `benches/`            | Runnable hosts (memory, file, SQLite), and the benchmarks                   |
| [`docs/DESIGN.md`](docs/DESIGN.md) | The specification                                                           |

Benchmarks write their results beside them, as JSON (`cargo bench --bench
checkpoints`, `cargo bench --features sqlite-host --bench sqlite`).

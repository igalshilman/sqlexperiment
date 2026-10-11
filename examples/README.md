# Examples

By language. `make examples` builds them all (see the [Makefile](../Makefile)).

## Rust

- [`rust/lookup.rs`](rust/lookup.rs): the core crate alone, over an in-memory
  store. `cargo run --features simulation --example lookup`.
- [`rust/ledger`](rust/ledger): a ledger virtual object on the
  [Restate Rust integration](../restate/README.md), and its crash campaign
  ([`tests/crash.rs`](rust/ledger/tests/crash.rs)) against a real Restate
  server, which runs the Node.js ledger too.

## Node.js

[`node/`](node/README.md), on the patched Restate TypeScript SDK and the
[addon](../node/README.md):

- [`ledger.mjs`](node/ledger.mjs): the same ledger.
- [`shop.mjs`](node/shop.mjs): warehouses and orders, each object with its own
  database, over idempotent RPC and delayed sends.
- [`agent.mjs`](node/agent.mjs): a durable agent on pi-durable, whose storage
  is the object's database.
- [`test.mjs`](node/test.mjs): all of them against a real Restate server,
  through crashes.

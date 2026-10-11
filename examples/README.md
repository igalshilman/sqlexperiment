# Examples

By language and library. `make examples` builds them all (see the
[Makefile](../Makefile)).

## Rust

- [`rust/lookup.rs`](rust/lookup.rs): the core crate alone, over an in-memory
  store. `cargo run --features simulation --example lookup`.

## JavaScript

One npm package, here, on the patched Restate TypeScript SDK and the
[addon](../node/README.md) (`make examples-js` installs it):

- [`node/`](node/README.md): SQL in Restate virtual objects, how it works,
  and two examples:
  - [`ledger.mjs`](node/ledger.mjs): accounts and transfers, deduped by
    transfer id;
  - [`shop.mjs`](node/shop.mjs): warehouses and orders, each object with its
    own database, over idempotent RPC and delayed sends.
- [`pi/`](pi/README.md): a durable agent on pi-durable, whose storage is the
  object's database, with a fake model for running agents without a key.
- [`serve.mjs`](serve.mjs): serves them in one endpoint (`npm run serve`).
- [`faults.mjs`](faults.mjs): fault injection for the crash test.
- [`test.mjs`](test.mjs): all of them against a real Restate server, through
  crashes.

## Validation

[`test.mjs`](test.mjs) runs every JavaScript example against a real server
through crashes. The ledger and the shop run in one process, aborting at 1.5%
of state requests; the agents run in another, aborting at 0.3% (a prompt makes
some 200 state requests, so one attempt in three crashes). The server is
killed twice. Meanwhile:
- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four agent sessions take eight transfer prompts each (a third sent twice)
  and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed twice, with 109 and 108 restarts of the shop's process and 85 and 92 of
the agents'.

```sh
RESTATE_SERVER=/path/to/restate-server make crash-test
```

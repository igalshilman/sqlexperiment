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
- [`langgraph/`](langgraph/README.md): a LangGraph agent, whose checkpoints
  are in the object's database, through a checkpointer that passes LangGraph's
  conformance suite.
- [`serve.mjs`](serve.mjs): serves them in one endpoint (`npm run serve`).
- [`faults.mjs`](faults.mjs): fault injection for the crash test.
- [`test.mjs`](test.mjs): all of them against a real Restate server, through
  crashes.

## Validation

[`test.mjs`](test.mjs) runs every JavaScript example against a real server
through crashes. The ledger and the shop run in one process, aborting at 1.5%
of state requests; the pi agents run in another and the LangGraph agents in a
third, aborting at 0.3% (a pi prompt makes some 200 state requests, so one
attempt in three crashes). The server is killed twice. Meanwhile:
- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four sessions of each agent take eight transfer prompts each (a third sent
  twice) and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed twice, with 109 and 108 restarts of the shop's process and 85 and 92 of
the agents', and again on the context with plain promises (81 and 111). With
the LangGraph agents too, it passed twice: 64 transfers, one per prompt, with
28 and 26 restarts of their process.

```sh
RESTATE_SERVER=/path/to/restate-server make crash-test
```

[`concurrency.mjs`](concurrency.mjs) tests the context without journal replay
against a real server, with the inactivity timeout at one second: plain
promises under `Promise.all`, `Promise.race` and a hand-written combinator; a
call that starts unawaited, and one awaited late beside a pending 30 s sleep;
`ctx.run` refused with a terminal error; failures, invocation ids, statements
beside a call and a sleep in `ctx.sqlSession`; a suspension resuming on the
first of two operations (the call at 3 s, not the sleep at 20 s); and a
cancellation reaching three pending operations. A handler that replays its journal keeps `RestatePromise`s.

```sh
RESTATE_SERVER=/path/to/restate-server make concurrency-test
```

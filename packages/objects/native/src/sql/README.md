# SQL

An object's database: SQLite-compatible SQL on the
[state](../state/README.md), for a Restate object without Restate's journal
(`journalReplay: false`), whose state the namespace is. It runs Turso's Rust
engine (`turso_core` 0.8.2) unmodified, over the state's one file, and commits
with the state: the rows and the effects, durable together in one flush.

A `Database` is one session of it. Its statements change a private view of
the file, and each call that leaves no SQL transaction open commits: what the
SQL committed, and the effects of `commit(effects)` if any, in one flush,
durable before the call returns. With a page cache, a session that ends
cleanly leaves its engine warm for the namespace's next one. It is a private
module of the addon's crate; where it sits: [the
layers](../../README.md#inside-the-layers).

## API

A host implements `state::kv::KvStore` over its store (the addon, over the
JavaScript one), then:

```rust,ignore
use crate::sql::Database;

// Kept warm in the process's page cache under the namespace's stable key.
let db = Database::open(store, "object-42", false).await?;
db.execute("UPDATE stock SET n = n - 1 WHERE id = 7").await?; // durable now
db.begin().await?;                                  // one transaction, with effects
db.execute("INSERT INTO shipments VALUES(7)").await?;
db.commit(Some("shipped 7".into())).await?;         // the row and the effects, durable together
db.save().await?;                                   // ends the session, leaving its engine warm
```

`transaction(body)` runs `body` between `begin` and `commit`, or rolls it
back when it fails; a statement in the body that ends its transaction
(`COMMIT`, `END`, `ROLLBACK`) ends the session rather than commit the body
halfway. Bind values with `query_with`/`execute_with` rather than formatting
them into SQL. `SqlOptions` caps a session's private changes and a
statement's result rows. `Database::new(state)` opens a session on a state
opened otherwise, as the tests and benchmarks do.

An SQL error (syntax, a constraint, a missing table) undoes just that statement,
as in SQLite, and the session goes on. A storage error, a cancelled call or a
failed commit ends the session (`is_usable()` returns false): settle
outstanding requests and open a new one; a failed commit's outcome is unknown.

The addon ([`node.rs`](../node.rs)) exposes a `Database` to JavaScript, over
a key/value store implemented there.

## Layout

| Path                                  | What                                                                                   |
| ------------------------------------- | -------------------------------------------------------------------------------------- |
| `mod.rs`, `io.rs`                     | `Database` and its engine, and the adapter (`io`) from the engine's files to the state |
| `tests/`                              | SQL, and the state's storage, crash and cache campaigns under SQL workloads            |
| [`benches/`](../../benches/README.md) | Benchmarks of SQL workloads on the state                                               |

## Build and test

From `packages/objects/native`:

```sh
cargo test --locked --release
cargo clippy --locked --all-targets -- -D warnings
```

The tests run on the state's in-memory KV store, fault injection and latency
simulator (its `simulation` module, always on in tests; the feature `bench`
turns it on for the benchmarks).

The randomized campaign (`tests/integration/campaign.rs`) runs a few seeds by
default; run more, or replay a failing one with a trace:

```sh
SQL_CAMPAIGN_SEEDS=1000 cargo test --locked --release sql::tests::integration::campaign
SQL_CAMPAIGN_SEED=17 SQL_CAMPAIGN_TRACE=1 cargo test --locked sql::tests::integration::campaign -- --nocapture
```

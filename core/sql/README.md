# SQL

SQLite-compatible SQL on the [state](../state/README.md), for a Restate object
without Restate's journal (`journalReplay: false`), whose state the namespace
is. It runs Turso's Rust engine (`turso_core` 0.8.2) unmodified, over the
state's file `db`, and flushes with the state: the rows, and the state's other
files and records, durable together in one flush.

One `Sql` runs on a state at a time. Its statements change a private view; `Sql::flush(&mut mutation)` moves what SQL committed into a mutation; the
mutation's `flush` makes it durable. With a page cache, a session that ends cleanly
leaves its engine warm for the namespace's next one.

## Use

Implement `state::kv::KvStore` over your store, then:

```rust,ignore
use state::{PageCache, PageCacheConfig, State};
use sql::Sql;

// Once per process: 256 MiB of pages in memory, 8 GiB in a temporary file.
let cache = PageCache::open(PageCacheConfig::new(256 << 20, 8 << 30)).await?;

// Per session, kept warm in the cache under the namespace's stable key.
let state = State::open(store).cached(&cache, "object-42").await?;
let mut db = Sql::open(&state).await?;
db.transaction(async |db| {                    // one SQL transaction, or rolled back
    db.execute("UPDATE stock SET n = n - 1 WHERE id = 7").await?;
    db.execute("INSERT INTO shipments VALUES(7)").await
}).await?;
let mut mutation = state.mutation();
db.flush(&mut mutation).await?;                     // the rows, into the mutation
mutation.set(&state.record("progress"), "shipped 7".into())?;
mutation.flush().await?; // the rows and the record, durable together
```

Nothing is durable before `Mutation::flush`, and every mutation of the state refuses to
flush while SQL holds changes not moved into one. A mutation dropped unflushed
discards its writes; SQL still holds the pages it moved there, and its next
`flush` moves them again. The host chooses when to flush:
after each SQL transaction, as the Node.js addon and the Python extension do,
or once per turn, as the durable functions runtime does. Bind values with
`query_with`/`execute_with` rather than formatting them into SQL.
`SqlOptions` caps a session's private changes and a statement's result rows.

An SQL error (syntax, a constraint, a missing table) undoes just that statement,
as in SQLite, and the session goes on. A storage error, a cancelled call or a
failed flush ends the session (`is_usable()` returns false): settle
outstanding requests and open a new one; a failed flush's outcome is unknown.

## Bindings

- [Node.js addon](../../typescript/packages/sql/README.md): SQL over a
  key/value store implemented in JavaScript, in `typescript/packages/sql/`
- [Python extension](../../python/sql/README.md): the same in Python, in
  `python/sql/`

Both expose `binding::Session`: a session that flushes SQL into a mutation, with
the `effects` record, and flushes the mutation after each call that leaves no SQL
transaction open, with the transaction calls behind their `transaction()`.

## Layout

| Path                  | What                                                                                                             |
| --------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `src/`                | The crate: `Sql`, the adapter (`io`) from the engine's files to the state, and the bindings' session (`binding`) |
| `tests/`              | SQL, and the state's storage, crash and cache campaigns under SQL workloads                                      |
| `examples/`           | `lookup`: a small database, opened and queried                                                                   |
| [`benches/`](benches) | Benchmarks of SQL workloads on the state, and their results                                                      |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # storage, cache and failure campaigns
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo run --locked --features simulation --example lookup
```

The `simulation` feature turns on the state's in-memory KV store, fault
injection and latency simulator, used by most tests and all benchmarks.

The randomized campaign (`tests/integration/campaign.rs`) runs a few seeds by
default; run more, or replay a failing one with a trace:

```sh
SQL_CAMPAIGN_SEEDS=1000 cargo test --locked --release --features simulation --test integration campaign
SQL_CAMPAIGN_SEED=17 SQL_CAMPAIGN_TRACE=1 cargo test --locked --features simulation --test integration campaign -- --nocapture
```

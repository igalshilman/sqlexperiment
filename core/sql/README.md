# SQL

SQLite-compatible SQL on a [journal](../journal/README.md), for a Restate object
without Restate's journal (`journalReplay: false`), whose state the journal's
namespace is. It runs Turso's Rust engine (`turso_core` 0.8.2) unmodified,
over the journal's file `db`, and commits with the journal: the rows, and the
journal's other files and records, durable together in one put.

SQL takes part in the journal as the participant `sql`. Its statements change a
private view; `flush` moves what SQL committed into the file `db`; the
journal's `commit` makes it durable. With a page cache, a session that ends
cleanly leaves its engine warm for the namespace's next one.

An experiment, not a production backend: see the journal's
[design](../journal/docs/DESIGN.md).

## Use

Implement `journal::kv::KvStore` over your store, then:

```rust,ignore
use journal::{Journal, PageCache, PageCacheConfig, StorageOptions};
use sql::Sql;

// Once per process: a page cache in memory and on a local disk file.
let cache = PageCache::open(PageCacheConfig::new("/var/cache/pages", 256 << 20, 8 << 30)).await?;

// Per session, with the namespace's stable key.
let options = StorageOptions {
    cache: Some(cache.namespace("object-42")),
    ..StorageOptions::default()
};
let journal = Journal::open_with_options(store, options).await?;
let mut db = Sql::open(&journal).await?;
db.transaction(async |db| {                    // one SQL transaction, or rolled back
    db.execute("UPDATE stock SET n = n - 1 WHERE id = 7").await?;
    db.execute("INSERT INTO shipments VALUES(7)").await
}).await?;
db.flush().await?;                             // into the file `db`, still private
journal.record("progress").set("shipped 7".into());
journal.commit().await?; // the rows and the record, durable together
```

Nothing is durable before `Journal::commit`, and the journal refuses to commit
while SQL holds changes not flushed. The host chooses when to commit: after
each SQL transaction, as the Node.js addon and the Python extension do, or once
per turn, as the durable functions runtime does. Bind values with
`query_with`/`execute_with` rather than formatting them into SQL.
`SqlOptions` caps a session's private changes and a statement's result rows.

An SQL error (syntax, a constraint, a missing table) undoes just that statement,
as in SQLite, and the session goes on. A storage error, a cancelled call or a
failed commit ends the session (`is_usable()` returns false): settle
outstanding requests and open a new one; a failed commit's outcome is unknown.

## Bindings

- [Node.js addon](../../typescript/packages/sql/README.md): SQL over a
  key/value store implemented in JavaScript, in `typescript/packages/sql/`
- [Python extension](../../python/sql/README.md): the same in Python, in
  `python/sql/`

## Layout

| Path                  | What                                                                            |
| --------------------- | ------------------------------------------------------------------------------- |
| `src/`                | The crate: `Sql`, and the adapter (`io`) from the engine's files to the journal |
| `tests/`              | SQL, and the journal's storage, crash and cache campaigns under SQL workloads   |
| `examples/`           | `lookup`: a small database, opened and queried                                  |
| [`benches/`](benches) | Benchmarks of SQL workloads on the journal, and their results                   |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # storage, cache and failure campaigns
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo run --locked --features simulation --example lookup
```

The `simulation` feature turns on the journal's in-memory KV store, fault
injection and latency simulator, used by most tests and all benchmarks.

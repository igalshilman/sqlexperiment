# SQLite KV

A Rust experiment running SQLite-compatible SQL over an asynchronous key/value
store, using the unmodified Rust engine implemented by Turso (`turso_core` 0.8.2).
It targets single-owner namespaces, such as one Restate virtual object: one owner
at a time runs sessions against a namespace, and each save commits durably to the
KV store.

SQL writes and the engine WAL stay private until `Session::save()`, which commits
the changed pages as one log transaction, usually a single put. Checkpoints later
move pages to stable keys, as SQLite does with its WAL.

## Use

Implement `kv::KvStore` with asynchronous `get`, `put` and `delete` over
`bytes::Bytes` values, then open a session:

```rust,ignore
use sqlite_kv::{PageCache, PageCacheConfig, Session, StorageOptions};

// Once per process: pages in memory and on a local disk file.
let cache = PageCache::open(PageCacheConfig::new("/var/cache/pages", 256 << 20, 8 << 30)).await?;

// Per session, with the namespace's stable key.
let options = StorageOptions {
    cache: Some(cache.namespace("object-42")),
    ..StorageOptions::default()
};
let mut db = Session::open_with_options(store, options).await?;
db.execute("CREATE TABLE t(n INTEGER)").await?;
db.execute("INSERT INTO t VALUES(1)").await?;
let saved = db.save().await?;
if let Some(error) = saved.cleanup_error {
    // Committed; only checkpoint work failed. Do not replay the INSERT.
    eprintln!("committed; checkpoint pending: {error}");
}
```

`Session::open(store)` works without a cache. `StorageOptions` also selects
`PageCompression::{Lz4, Raw}` (default adaptive LZ4) and `checkpoint_after`
(logged transactions before saves checkpoint, default 4).

## How it works

**Keys.** `v12/meta` records where the log starts. `v12/log/<n>` holds log
segments, and `v12/page/<n>` holds checkpointed pages, overwritten in place. A
page's value is its latest image in the log, else its page key, so a cold read is
one get.

**Save.** A save writes its changed pages as one log transaction; the put of its
commit segment commits it. In the same round trip it may checkpoint the oldest
logged transactions (about 12 pages) into their page keys, and delete segments that a
durable `meta` has moved past. `meta` moves past a transaction only after all its
pages are acknowledged. A save over 256 KiB checkpoints itself right after
committing, so later opens do not replay it.

**Open.** Open reads `meta`, then replays committed transactions from its log
start, stopping at the first absent key or at debris from a failed save. On
backends where a failed put can be visible without being durable, open re-puts
the log tail and `meta` it did not write itself. Backends whose visible values
are always durable say so with `KvStore::durable_when_visible`.

**Cache.** `PageCache` is created once per process. It keeps pages in memory and
on a local disk tier ([foyer](https://github.com/foyer-rs/foyer)), shared by every
namespace under one budget. `cache.namespace(key)` returns that namespace's
handle, and namespaces never read each other's pages. Per namespace, the cache
remembers what this owner made durable, so a steady-state open and save are one
round trip each. A session that ends cleanly leaves its engine parked; the next
session reuses it when nothing changed, which costs about 5× less CPU. Parked
engines are capped across the process.

When another writer checkpoints, open refreshes the cached pages it rewrote
from its log segments, which are still present unless garbage collection
already deleted them. If they are gone, the namespace drops its cached pages.
The disk tier starts empty in every process.

**Execution.** The host task drives engine open, SQL steps and KV I/O directly.
Cached queries yield every 64 engine steps. The only background work is the
cache's disk I/O, which runs on the host's tokio runtime, or on a one-thread
runtime of its own when there is none. The engine I/O pump overlaps adjacent
reads in batches of up to 16, without crossing write, truncate or sync barriers.

## Sessions and errors

SQL `COMMIT` and `checkpoint()` affect only the private view. `save()` consumes
the session and is the durability boundary; dropping a session discards unsaved
work. Query errors and cancellation poison the session.

`Ok(SaveReport)` confirms the commit; its `cleanup_error` reports failed
checkpoint work, which a later save retries. Any other error or cancellation can
leave the commit outcome unknown: settle outstanding requests and reopen.
Application operation IDs can resolve uncertain non-idempotent work.

## Backend and ownership contract

- A get returns the latest visible whole value, or absence.
- Successful puts and deletes are atomic, durable and visible after recovery.
  Deleting a missing key succeeds. No multi-key transaction or sync call is
  needed.
- Errors and cancellation may leave no effect, a visible-only effect or a
  durable effect, but never a torn value.
- The host enforces exclusive ownership and settles every outstanding request
  before recovery or ownership transfer. Dropping a future does not fence a
  remote request.
- Each namespace uses one stable key with `PageCache::namespace`.

The adapter provides no distributed lock or fencing. It is not a Restate SDK
integration or a production backend.

## Build and test

```sh
cargo test --locked
cargo test --locked --features simulation
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
cargo doc --locked --no-deps --all-features --document-private-items
cargo run --locked --features simulation --example lookup
```

| Location | Purpose | Feature |
|---|---|---|
| `src/` | Adapter, codecs, log, checkpoint protocol and cache | None |
| `src/simulation/` | In-memory KV, fault injection, latency simulation, test caches | `simulation` |
| `tests/public_api.rs` | Public API against an immediately ready backend | None |
| `tests/unit/` | Private implementation tests | Codec and log tests: none; the rest: `simulation` |
| `tests/integration/` | SQL, block I/O, cache, and save and open failure campaigns | `simulation` |
| `benches/` | Benchmarks, analysis scripts and recorded results | `simulation` |
| `examples/lookup.rs` | Small simulator-backed example | `simulation` |

Benchmarks run separately; see [benches/README.md](benches/README.md).

## Limits

- Logical pages are 4096 bytes; encoded pages are at most 4099 bytes.
- Log segments are at most 1 MiB, and larger saves span several. The backend
  must accept that value size.
- Dirty pages, the private WAL, the replayed log and query results have no
  memory budgets.
- One engine step cannot be preempted, and queries are not sandboxed.
- There is no payload checksum, corruption repair or attached-database support.
- Only the current storage format (v12) is supported.
- Measurements use an in-memory simulator. They count KV traffic and round
  trips, but do not model real backend latency, replication or physical storage.
  There are no process-kill or power-loss tests.

## Further reading

- [Storage format](docs/FORMAT.md)
- [Commit, checkpoint and recovery protocol](docs/PROTOCOL.md)
- [Review and validation](docs/REVIEW.md)
- [Benchmarks](benches/README.md): [log shipping](benches/LOG_RESULTS.md) and
  [cache](benches/CACHE_RESULTS.md) results

The rustdoc in `src/lib.rs` describes the architecture; `src/log.rs`,
`src/snapshot.rs`, `src/cache.rs` and `src/kv.rs` document the log, the save
pipeline, the cache and the backend contract.

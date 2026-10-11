# SQLite KV

A Rust experiment running SQLite-compatible SQL over an asynchronous key/value
store, using the unmodified Rust engine implemented by Turso (`turso_core` 0.8.2).
One owner serializes sessions in a dedicated namespace. SQL writes and the engine
WAL stay private until `Session::save()` commits the changed pages as one durable
log transaction, usually a single put. Checkpoints later move pages to stable keys,
as SQLite does with its WAL.

The default library contains the adapter and storage protocol. Simulation,
fault injection, counters and artificial scheduling yields are opt-in.

## Use

Implement `kv::KvStore` with asynchronous `get`, `put` and `delete`, then pass an
`Arc<dyn KvStore>` to `Session::open`:

```rust,ignore
use sqlite_kv::Session;

let mut db = Session::open(store).await?;
db.execute("CREATE TABLE t(n INTEGER)").await?;
db.execute("INSERT INTO t VALUES(1)").await?;
let saved = db.save().await?;
if let Some(error) = saved.cleanup_error {
    // Committed; only checkpoint work failed. Do not replay the INSERT.
    eprintln!("committed; checkpoint pending: {error}");
}
```

A save appends its changed pages to the log; the commit segment's put commits it.
In the same round trip it may checkpoint the oldest logged pages into
`page/<n>` keys and delete checkpointed segments. Pages are found in the replayed
log, else at their own key, so a cold read is one get. There is no page map,
object allocation or intent. `StorageOptions` selects
`PageCompression::{Lz4, Raw}` (default adaptive LZ4), `checkpoint_after` (logged
transactions before saves checkpoint, default 4) and an optional `PageCache`
(pages as stored, or `PageCache::decoded`; see [cache results](benches/CACHE_RESULTS.md)):
owner-process state that caches pages, resets itself when another writer moves
`meta`, and remembers what this owner made durable. With it, a steady-state open
and save are one round trip each, and a session that ends cleanly leaves its
engine for the next one, which reuses it when nothing changed (about 5× less CPU
per session). Backends whose visible values are always
durable can say so with `KvStore::durable_when_visible`; otherwise a fresh open
re-puts the log tail and `meta` it did not write. See
[the results](benches/LOG_RESULTS.md) for measurements against the previous design.
The engine I/O pump overlaps adjacent reads in batches of up to 16, with
write/truncate/sync barriers.

The host task drives engine open/connect, SQL steps and async KV I/O directly.
There are no worker threads or background tasks. Cached queries yield every 64
engine steps. The header page is loaded asynchronously and retained for the
engine's synchronous metadata reads. Other synchronous reads requiring KV or
synchronous file mutations return an error; the caller must discard that session.
The normal query and checkpoint paths use asynchronous I/O.

SQL COMMIT and `checkpoint()` affect the private view. `save()` consumes the
session and is the durability boundary. Dropping a session discards unsaved work.
Query errors and cancellation poison the session. `Ok(SaveReport)` confirms the
commit; its `cleanup_error` reports failed checkpoint work, which a later save
retries. An ordinary error or cancellation can leave an unknown commit outcome. Settle requests
and reopen; application operation IDs can resolve uncertain non-idempotent work.

## Backend and ownership contract

- A get returns the latest visible whole value or absence.
- Successful puts and deletes are atomic, durable and visible after recovery.
  Deleting a missing key succeeds. No multi-key transaction or sync call is needed.
- Errors and cancellation may leave no effect, a visible-only effect, or a durable
  effect. No torn values are allowed; success on one key need not flush another.
- The host enforces exclusive ownership and settles
  every outstanding request before recovery or ownership transfer. Dropping a
  future alone does not fence a remote request.

The adapter provides no distributed lock or fencing. It is not a Restate SDK
integration or a production backend.

## Build and test

```sh
cargo check --locked
cargo test --locked
cargo test --locked --features simulation
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
cargo doc --locked --no-deps --document-private-items
```

| Location / target | Purpose | Feature |
|---|---|---|
| `src/` | Adapter, codecs, log and checkpoint protocol | None |
| `src/simulation/` | MemoryKv, fault injection, latency and measurement | `simulation` |
| `tests/public_api.rs` | Real adapter with an immediately-ready test backend | None |
| `tests/unit/` | Private implementation tests and test-only helpers | Codec tests: none; storage tests: `simulation` |
| `tests/integration/` | SQL, block I/O, cache and save/open failure campaigns | `simulation` |
| `benches/` | Explicit Cargo bench targets, analysis scripts and results | `simulation` |
| `examples/lookup.rs` | Small simulator-backed usage example | `simulation` |

Unit test files are mounted with `#[cfg(test)]` module declarations to retain
private access. Tests and checkpoint fault hooks are excluded from normal library
builds. Default tests exercise the codec and public API without enabling simulation.
Benchmarks are run separately; see [benches/README.md](benches/README.md).

```sh
cargo run --locked --features simulation --example lookup
cargo bench --locked --features simulation --bench page_compression -- 3
```

## Storage and recovery

Logical pages are 4096 bytes. Encoded pages use adaptive LZ4 with a raw fallback,
at most 4099 bytes including the header, both inside log segments and at page
keys. A session keeps one decoded page and the header page. Only the current
format (v12) is supported, with no probing or handling of other formats.

Recovery is replay. Open reads `meta`, replays committed transactions from its
log start and stops at the first absent key or debris from a failed save; on weak
backends it confirms the tail it did not write. Checkpoints are idempotent page
puts; `meta` only moves past a transaction once its pages are acknowledged, and
segments are only deleted once a durable `meta` has moved past them. A large save
(over 256 KiB of segments) checkpoints itself right after committing.

See [storage format](docs/FORMAT.md), [commit/checkpoint protocol](docs/PROTOCOL.md)
and [self-review](docs/REVIEW.md). In-source rustdoc explains the architecture in
`src/lib.rs`, the commit records in `src/log.rs`, the save pipeline in
`src/snapshot.rs`, the cache in `src/cache.rs` and the backend contract in
`src/kv.rs`.

## Limits

Segments are at most 1 MiB; larger saves span several. The backend must accept
that value size. Dirty pages, private WAL, the replayed log and collected query
results have no byte budgets. One engine step is not forcibly preemptible, and
queries are not sandboxed. There is no payload checksum, corruption repair,
attached-database guarantee, or process-kill/power-loss test. A `PageCache` must
not be shared between namespaces: entries are keyed by page number only.

Measurements use an in-memory simulator. They count logical KV traffic and isolated
codec time, without modeling real backend latency, replication or physical storage.

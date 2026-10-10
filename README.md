# SQLite KV

A Rust experiment running SQLite-compatible SQL over immutable pages fetched on demand, using
the unmodified Rust engine implemented by Turso (`turso_core` 0.8.2).
One owner serializes sessions in a dedicated namespace. SQL writes and the WAL
stay private until `Session::save()` checkpoints and publishes a new snapshot
through one atomic durable head replacement.

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
    // Publication succeeded. Reopen to retry cleanup; do not replay the INSERT.
    eprintln!("committed; cleanup pending: {error}");
}
```

The page map is a radix tree with its root embedded in the snapshot head.
There is no map selection option. A save runs ordered phases; puts or deletes
within one phase target distinct keys and run concurrently (up to 16), and each
phase is fully acknowledged before the next. `StorageOptions` selects
`PageCompression::{Lz4, Raw}` (default adaptive LZ4), applied only to new pages;
stored tags determine decoding. `StorageOptions::cache` takes an optional
`ObjectCache`, a byte-bounded LRU of committed objects shared by successive
sessions of one namespace. Objects are immutable, so entries never go stale; a
warm reopen fetches only the head and intent.
`StorageOptions::max_log_segments` (default zero) enables a durable delta log:
a save commits by appending one segment of changed pages, a single put, and
every `max_log_segments + 1` saves fold the log into the map. Opens then cost
one extra round trip to confirm the last segment. See the
[delta-log results](benches/LOG_RESULTS.md) for the trade-off.
Independent head/intent reads overlap during open. The engine I/O pump overlaps
adjacent reads in batches of up to 16, with write/truncate/sync barriers.

The host task drives engine open/connect, SQL steps and async KV I/O directly.
There are no worker threads or background tasks. Cached queries yield every 64
engine steps. The header page is loaded asynchronously and retained for the
engine's synchronous metadata reads. Other synchronous reads requiring KV or
synchronous file mutations return an error; the caller must discard that session.
The normal query and checkpoint paths use asynchronous I/O.

SQL COMMIT and `checkpoint()` affect the private view. `save()` consumes the
session and is the durability boundary. Dropping a session discards unsaved work.
Query errors and cancellation poison the session. `Ok(SaveReport)` confirms
publication; its `cleanup_error` reports unfinished reclamation. An ordinary
error or cancellation can leave an unknown publication outcome. Settle requests
and reopen; application operation IDs can resolve uncertain non-idempotent work.

## Backend and ownership contract

- A get returns the latest visible whole value or absence.
- Successful puts and deletes are atomic, durable and visible after recovery.
  Deleting a missing key succeeds. No multi-key transaction or sync call is needed.
- Errors and cancellation may leave no effect, a visible-only effect, or a durable
  effect. No torn values are allowed; success on one key need not flush another.
- The host enforces exclusive ownership, excludes old snapshot readers, and settles
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
| `src/` | Adapter, codecs and publication protocol | None |
| `src/simulation/` | MemoryKv, fault injection and measurement | `simulation` |
| `tests/public_api.rs` | Real adapter with an immediately-ready test backend | None |
| `tests/unit/` | Private implementation tests and test-only helpers | Codec tests: none; storage tests: `simulation` |
| `tests/integration/` | SQL, block I/O, publication and recovery campaigns | `simulation` |
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

Logical pages are 4096 bytes. Encoded page objects use adaptive LZ4 with a raw
fallback, at most 4099 bytes including the header. A snapshot-local cache retains
one decoded page, alongside the retained header page. The map uses compact radix nodes; unchanged subtrees are reused.
Only the current format is supported, with no probing or handling of other formats.

The embedded root removes a separate map fetch from cold reads and avoids writing
and retiring a separate root object. Changed paths are rewritten immediately;
unchanged subtrees are reused and replaced objects are reclaimed after publication.
There are no override lists or deferred folds. A large root increases the bytes
in the head and intent; the design favors fewer requests and one simple lifecycle.

A fixed pending-save record lists created and retired object IDs before any object
writes. Save waits for new objects to be durable, replaces the head, then deletes
retired objects and the intent. Open settles an interrupted save by matching the
head against the intent's exact old/new heads. There is no scan-based GC.

See [storage format](docs/FORMAT.md), [save/recovery protocol](docs/PROTOCOL.md),
and [self-review](docs/REVIEW.md). In-source rustdoc explains the architecture
and alternatives in `src/lib.rs`, the structures and layout in `src/snapshot.rs`
and `src/map_codec.rs`, and the commit/recovery contract in `src/pending.rs` and
`src/kv.rs`.

## Limits

The pending record is limited to 1 MiB; an oversized save fails before mutations.
The backend must accept that value size and page objects up to 4099 bytes. Dirty
pages, private WAL, map cache, write plans and collected query results have no byte
budgets. One engine step is not forcibly preemptible, and queries are not sandboxed. There is no payload checksum, corruption repair,
attached-database guarantee, or process-kill/power-loss test. An `ObjectCache`
must not be shared between namespaces: entries are keyed by object ID only.

Measurements use an in-memory simulator. They count logical KV traffic and isolated
codec time, without modeling real backend latency, replication or physical storage.

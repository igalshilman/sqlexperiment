# Turso over asynchronous KV

A Rust experiment running unmodified Turso core 0.8.2 over lazy immutable pages.
One owner serializes sessions in a dedicated namespace. SQL writes and the WAL
stay private until `Session::save()` checkpoints and publishes a new snapshot
through one atomic durable head replacement.

The default library contains the adapter and storage protocol. Simulation,
fault injection, counters and artificial scheduling yields are opt-in.

## Use

Implement `kv::KvStore` with asynchronous `get`, `put` and `delete`, then pass an
`Arc<dyn KvStore>` to `Session::open`:

```rust,ignore
use turso_lazy_kv::Session;

let mut db = Session::open(store).await?;
db.execute("CREATE TABLE t(n INTEGER)").await?;
db.execute("INSERT INTO t VALUES(1)").await?;
let saved = db.save().await?;
if let Some(error) = saved.cleanup_error {
    // Publication succeeded. Reopen to retry cleanup; do not replay the INSERT.
    eprintln!("committed; cleanup pending: {error}");
}
```

`StorageOptions` selects `MapLayout::{Radix, InlineRoot, HeadOverrides}`, bounded
write concurrency, and `PageCompression::{Lz4, Raw}`. Defaults are Radix, 16
concurrent object mutations and adaptive LZ4. Existing heads select their stored
map layout. The compression policy applies only to new pages; each stored page
carries its own codec tag.

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
```

| Location / target | Purpose | Feature |
|---|---|---|
| `src/` | Adapter, codecs and publication protocol | None |
| `src/simulation/` | MemoryKv, fault injection and measurement | `simulation` |
| `tests/public_api.rs` | Real adapter with an immediately-ready test backend | None |
| `tests/unit/` | Private implementation tests and test-only helpers | Codec tests: none; storage tests: `simulation` |
| `tests/integration/` | SQL, block I/O, publication and recovery campaigns | `simulation` |
| `benches/` | Explicit Cargo bench targets, analysis scripts and results | `simulation` |
| `examples/lazy_lookup.rs` | Small simulator-backed usage example | `simulation` |

Unit test files are mounted with `#[cfg(test)]` module declarations to retain
private access. Tests and checkpoint fault hooks are excluded from normal library
builds. Default tests exercise the codec and public API without enabling simulation.
Benchmarks are run separately; see [benches/README.md](benches/README.md).

```sh
cargo run --locked --features simulation --example lazy_lookup -- radix
cargo bench --locked --features simulation --bench page_compression -- 3
```

## Storage and recovery

Logical pages are 4096 bytes. Encoded page objects use adaptive LZ4 with a raw
fallback, at most 4099 bytes including the header. A snapshot-local cache retains
one decoded page. The map uses compact radix nodes; unchanged subtrees are reused.
Only the current format is supported, with no probing or handling of other formats.

| Map layout | Structure | Tradeoff |
|---|---|---|
| Radix | External adaptive-height 256-way tree | Rewrites affected paths |
| InlineRoot | Top node embedded in head | Saves a root request; enlarges head and intent |
| HeadOverrides | External tree plus at most 64 overrides | Defers map rewrites and retains up to 64 shadowed base pages |

A fixed pending-save record lists created and retired object IDs before any object
writes. Save waits for new objects to be durable, replaces the head, then deletes
retired objects and the intent. Open settles an interrupted save by matching the
head against the intent's exact old/new heads. There is no scan-based GC.

See [storage format](docs/FORMAT.md), [save/recovery protocol](docs/PROTOCOL.md),
and [self-review](docs/REVIEW.md).

## Limits

The pending record is limited to 1 MiB; an oversized save fails before mutations.
The backend must accept that value size and page objects up to 4099 bytes. Dirty
pages, private WAL, map cache, write plans and collected query results have no byte
budgets. There is one native engine worker per session. Queries are not sandboxed
or forcibly preempted. There is no payload checksum, corruption repair,
cross-session cache, attached-database guarantee, or process-kill/power-loss test.

Measurements use an in-memory simulator. They count logical KV traffic and isolated
codec time, without modeling real backend latency, replication or physical storage.

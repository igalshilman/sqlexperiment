# Turso over lazy K/V — single pending-save record

This initial repository import is the saved 0.9.0 benchmark/failpoint checkpoint
(storage format v7). The later experimental v8 cache/compact-map optimization
source could not be recovered for this import and is not included. Test and
benchmark logs under `results/` are the recorded checkpoint results; they were
not rerun during import because the import environment had no Rust toolchain.

Unmodified Turso core 0.8.2 over asynchronous lazy K/V, with 4 KiB database pages
and immutable objects. One owner serializes sessions per namespace. The engine's
WAL stays in a private in-memory overlay. `Session::save()` checkpoints privately,
then commits the resulting snapshot through one atomic durable head replacement.

A fixed `pending_save` key records new and retired object IDs **before any object
writes**. Open resolves an interrupted save before exposing a session. There is
no scanning GC, scan API, timer, external retirement list, allocation reservation,
raw-K/V sync, or engine patch. This is an isolated Rust prototype, not a Restate
SDK integration or production backend.

## Run

```sh
CARGO_INCREMENTAL=0 cargo test --locked -- --nocapture
CARGO_INCREMENTAL=0 cargo run --locked --release --example small_databases -- 3
cargo run --locked --example lazy_lookup -- radix
CARGO_INCREMENTAL=0 cargo run --locked --release --example amplification -- 3 > results/amplification.csv
python3 scripts/analyze_amplification.py
```

The lazy-lookup example also accepts `inline` and `overrides`.

```rust,ignore
use turso_lazy_kv::{Session, StorageOptions, MapLayout};
let mut db = Session::open_with_options(store.clone(), StorageOptions {
    layout: MapLayout::Radix,
    write_concurrency: 16,
}).await?;
db.execute("CREATE TABLE t(n INTEGER)").await?;
db.execute("INSERT INTO t VALUES(1)").await?;
let saved = db.save().await?;
if let Some(error) = saved.cleanup_error {
    // SQL is committed. Reopen to retry maintenance; do not replay this INSERT.
    eprintln!("committed; cleanup will retry on open: {error}");
}
```

`Ok(SaveReport)` confirms durable publication. Its `cleanup_error` field reports
unfinished reclamation. An ordinary `Err` or cancellation can leave an unknown
publication outcome; settle requests and reopen. An application operation ID can
resolve uncertain non-idempotent commands. Save consumes the session.

## Required backend and host semantics

- `get(key)`: latest visible whole value or absence.
- `put(key, value)`: atomic replacement; success means this key's value is durable
  and visible to later reads, including after recovery.
- `delete(key)`: atomic deletion; success means absence is durable. Missing keys
  are a successful idempotent delete.

Errors and cancellation may have no effect, a visible but not durable effect,
or a durable effect. No torn values are allowed. A successful mutation need not
persist earlier unacknowledged mutations on other keys. No multi-key atomicity or
separate sync barrier is required.

The host enforces exclusive ownership and settles **all** previous requests,
including cancelled ones, before recovery or ownership transfer. No historical
readers may retain an old snapshot across save. The adapter provides no distributed
lock or fencing. The namespace must be dedicated to this adapter.

`MemoryKv` models visible/durable maps in process memory. Fault hooks explicitly
create unacknowledged effects; these are not successful K/V operations. Its `keys`
and `stored_bytes` methods are diagnostics, not required backend operations.

## Save and recovery

1. Reject an open SQL transaction; complete and validate TRUNCATE checkpoint.
2. Stop/join the worker. Plan final database pages, rewritten map nodes, and
   retired IDs in memory. Prune temporary nodes that are absent from the final map.
3. Durably put `pending_save`, including exact old/new head bytes and both ID lists.
4. Put all new objects with bounded concurrency; await every durable acknowledgment.
5. Durably replace `head`: this is the commit point.
6. Delete retired objects with bounded concurrency, then durably delete `pending_save`.

Open reads the fixed pending key and head. With no pending record it opens normally.
With a record, the head must equal exactly the old or new head. Before deleting,
recovery durably confirms the selected head (or durably deletes head for an empty
old state). Old selected: delete attempted new objects. New selected: delete retired
objects. Only after all deletions succeed does recovery delete the pending record.
A mismatch or malformed record stops recovery without mutations. A failed recovery
must finish on a later open before another save can start.

Ordinary object errors drain outstanding work and skip publication. Ordinary delete
errors drain work and preserve the intent. Cancellation still requires host request
settlement; dropping a future cannot cancel an already dispatched remote mutation.
A failure reading the pending record or head prevents recovery mutations.
Cleanup errors after commit are reported
as successful publication with pending maintenance.

See [GC_DESIGN.md](GC_DESIGN.md) for the crash table, metadata format and costs.

## Storage format and limits

Format v7 uses `v7/head`, `v7/pending_save`, `v7/node/<u64-hex>` and
`v7/block/<u64-hex>`. Block objects are 4096 bytes; external radix nodes are sorted
one-byte edge/eight-byte child-ID pairs (9 bytes each, at most 2304 bytes).

The head starts with `TKV7`, database byte length (u64 LE), last allocated ID
(u64 LE), height (u8), layout (u8), root-section length (u16 LE), and override count
(u16 LE): 26 bytes. An external root section is an 8-byte ID (zero means empty).
InlineRoot embeds node entries. Each override is a logical block number (u64) and
object ID (u64); zero means a tombstone. No pending-deletion list lives in the head.

IDs are incrementing u64 values shared by page and map-node objects, never hashes.
IDs above the recovered committed counter may be reused **after aborted-attempt
cleanup**. Published IDs are not reused. Planning gaps are allowed. Overflow fails
before any backend mutation. Older persisted formats, including v6, require a
fresh namespace; migration and cleanup of pre-v7 leaks are not implemented.

A pending record is limited to **1 MiB**, exposed as `MAX_PENDING_BYTES`. An
oversized save fails before its first mutation. Backend values must support that
limit plus 4 KiB pages. There is no record chunking. This limits changed-object
metadata per save, not total database size. The private WAL, dirty overlay, plan,
query rows and caches remain without byte budgets.

## Existing map alternatives

| Layout | Structure | Main tradeoff |
|---|---|---|
| Radix (default) | External adaptive-height 256-way tree; 34-byte head | Rewrites affected paths; unchanged subtrees reused |
| InlineRoot | Top node embedded in head, at most 2330 bytes | Saves root get/put but larger head is also copied into intent |
| HeadOverrides | External base plus at most 64 head overrides; head at most 1058 bytes | Avoids map rewrites until a fold; larger head/intent and delayed base-page reclamation |

Radix level one covers 1 MiB of dense addresses, level two 256 MiB, and level three
64 GiB. Growth/collapse handles leading-zero paths; it is not general path compression.

HeadOverrides checks the bounded overlay before the base map. The 65th distinct
entry, or database shrink, folds all overrides into the base. Replaced override
objects are deleted on save. A shadowed base page remains structurally referenced
until folding, with at most 64 shadowed pages. This bounded retention is deliberate;
it requires no scanner. Layout is selected on first open and persisted thereafter.
There is no online conversion API.

## Engine integration

Public Turso `IO`/`File`, `Database::do_open_async`, storage options,
`wal_auto_actions_disable`, `TempStore::Memory` and checkpoint SQL are used without
patching upstream. The engine is pinned to 0.8.2. Blocking open/connect runs on a
native worker; the host asynchronously services lazy K/V reads and private writes.
Success completion callbacks run on that worker through `IO::step`, keeping engine
polling and completion-group finalization on one thread.

SQL COMMIT and explicit `checkpoint()` affect only the private view. Multiple SQL
commits may be grouped into one save. Do not acknowledge durable application state
before publication succeeds. Engine `File::sync` is a local overlay barrier, not a
backend call. Dropping a session discards its unsaved overlay. Query errors or
cancellation poison the session. Each open creates a fresh engine/cache.

## Evidence and next work

[AMPLIFICATION.md](AMPLIFICATION.md) defines phase/object counters, failpoints,
workloads and amplification denominators. [AMPLIFICATION_RESULTS.md](AMPLIFICATION_RESULTS.md)
reports the extended measurements; raw and summarized CSVs are under `results/`.

[VALIDATION.md](VALIDATION.md) records executed checks and interruption coverage.
[BENCHMARKS.md](BENCHMARKS.md) and `results/small_databases.csv` report small SQL
workloads and a larger control. [SELF_REVIEW.md](SELF_REVIEW.md) records review
findings and remaining risks. Compression, compact maps and cross-session LRU remain
notes only in [DEFERRED_OPTIMIZATIONS.md](DEFERRED_OPTIMIZATIONS.md).
[MVCC.md](MVCC.md) preserves a separate deferred design; RocksDB is not assumed.

Next: real host backend and request fencing, network-latency measurements, memory
budgets, and an explicit maximum-save policy. There is no SQL sandbox, attached-DB
guarantee, payload checksum, corruption repair, cross-session cache or power-loss test.

Upstream: https://github.com/tursodatabase/turso/tree/v0.8.2

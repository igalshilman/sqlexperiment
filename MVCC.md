# Deferred alternative: committed snapshots and atomic batches

The current format-v7 implementation uses raw K/V with atomic durable puts and
deletes. RocksDB and MVCC are set aside; they are not requirements for that design.
This document preserves the earlier exploration of a stronger host interface and
is not an implemented backend or the current next step.

## Host interface

Conceptually:

```text
snapshot = open_snapshot(object_namespace)
get(snapshot, logical_key) -> value
commit(object_namespace, ownership_token, mutations, operation_id) -> durable result
release_snapshot(snapshot)
```

The host must expose batch atomicity through its replication/application path,
not merely happen to use RocksDB behind an API with independent per-key puts.
Commit success acknowledges the host's replicated durability. RocksDB's local
`WriteOptions.sync=true` alone does not establish a distributed durability guarantee.
A live ownership token/fence prevents a stale writer from committing.

RocksDB provides point-in-time snapshots, snapshot-bound Get/MultiGet, and atomic
WriteBatch application through public APIs. Single ownership avoids concurrent
writer conflict detection for this namespace; it does not eliminate the need for
atomic batch publication. MVCC mainly adds stable reader views and version retention.

## Direct logical page keys

Store:

- `<object>/db/page/<block_number>`: 4096-byte page/block value.
- `<object>/db/length`: database byte length.
- Optional operation-result record, atomically committed with the data.

There are no application-level immutable object IDs, page-to-object radix map,
root pointer, head overrides, attempt-version keys, commit-marker table or custom
object mark-and-sweep. The block number can remain a u64; the host's MVCC sequence
is an implementation detail rather than our object allocator.

A SQL session pins a host snapshot. Reads check its private dirty overlay first,
then Get the logical page under that snapshot. All pages and length come from the
same snapshot. One missing page is one host get, without an adapter map traversal.
MultiGet can combine known independent requests. This does not turn dependent
SQLite B-tree traversal into parallel page reads automatically.

## Save

1. Finish the SQL transaction and checkpoint the private Turso WAL into the local
   database overlay. Validate checkpoint completion and stop the engine as today.
2. Build one batch with final dirty page values, required deletions/tombstones,
   updated file length and optional operation-result record.
3. Submit one atomic durable host commit and acknowledge after it succeeds.
4. Release the session snapshot; acquire a new one for subsequent work.

D dirty pages mean D page puts inside one atomic batch, plus changed metadata and
truncation deletes. There are no adapter map-node writes and no separate head
publication barrier. The interface exposes one durable commit, not a promise of
one physical fsync or one network round trip. If the SDK streams large batches,
it needs isolated staging followed by the same atomic durable commit boundary.
Batch size limits must be represented without silently splitting an atomic save
into independently published chunks.

A successful commit recovers as the complete new database. An unknown commit
outcome can be old or new but cannot expose a partial page set. An operation ID
stored within the same batch can resolve retries. SQL COMMIT remains private until
host commit, unless a future wrapper intentionally changes that boundary.

Truncation must delete/tombstone removed keys together with length, including
correct handling of a partial last block. Otherwise old contents may reappear on
regrowth. The adapter already builds a dirty overlay for these file operations;
that overlay can feed the batch.

## WAL, retention and recovery

The engine continues to use a private local WAL for SQL transactions, rollback and
savepoints. Storage-layer MVCC does not automatically disable Turso's WAL. It is
still never sent as a separate application WAL to Restate. RocksDB's own durability
log and LSM flush/compaction are backend costs; direct keys do not eliminate them.

RocksDB retains old versions needed by active snapshots and can reclaim obsolete
versions through compaction after snapshots are released. Long-lived snapshots
can delay reclamation. The wrapper needs neither the immutable-page map nor its pending-save record.
This is backend-managed reclamation, not zero storage amplification.

Ordinary RocksDB snapshots are process-local and do not survive DB restart. They
are appropriate for an active session. After host failure, discard the unfinished
session and open a new snapshot of the recovered committed database. Resuming an
old historical snapshot across failover would require an additional durable
historical-version feature, which is not assumed here. A new owner must never
reuse a dead process's snapshot handle as a durable identifier.

The native worker, public Turso IO adapter, lazy page reads, local WAL/overlay,
SQL rollback behavior and operation-outcome handling remain reusable. A new storage
adapter can replace Snapshot's map-based block lookup/publication with snapshot Get
and host atomic batch commit. The current map variants provide a measured baseline for the raw exact-key API.

## Primary references

These support RocksDB capabilities, not an assertion that a particular Restate SDK
already exposes the proposed interface:

- https://github.com/facebook/rocksdb/wiki/Basic-Operations
- https://github.com/facebook/rocksdb/wiki/Snapshot
- https://github.com/facebook/rocksdb/wiki/RocksDB-Overview

# Measuring the shim before adding a real backend

Crate 0.9.0 retains the v7 storage protocol and unmodified Turso 0.8.2. This phase
adds instrumentation, failpoints and workload coverage, without changing the map,
cache, commit protocol, or durability contract. Results are logical operation and
byte counts. This is not a network, replication, disk or throughput benchmark.

## Reproduce

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test --locked -- --nocapture
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo run --locked --release --example amplification -- 3 > results/amplification.csv
python3 scripts/analyze_amplification.py
```

The analyzer checks exact agreement across repeats and checks that object-class
counters sum to the totals. It writes `results/amplification_summary.csv` and
[AMPLIFICATION_RESULTS.md](AMPLIFICATION_RESULTS.md). The previous benchmark and
its 0.8.0 results remain in `BENCHMARKS.md` and `results/small_databases.csv`.

## What is counted

`MemoryKv::stats().by_kind` partitions operations into database blocks, map nodes,
head, pending intent, and other keys. For each class it records get/put/delete
counts, get misses, value bytes read/written/reclaimed, and the key bytes sent by
each operation. Existing aggregate counters remain available. Diagnostic sets
count distinct keys within the measurement phase. A repeated read of one page is
two gets but one distinct page. A missing get has request key bytes and zero value
bytes. Deleting an absent key still has one delete and key bytes, but reclaims zero.

The raw CSV has six rows per measured phase: one total plus five object classes.
Do not add the total to its components. The benchmark resets counters between:

- **Open**: pending/head reads, recovery if needed, and engine initialization.
- **Query or SQL**: actual statement work after open.
- **Checkpoint**: moving private WAL state into private database pages.
- **Save**: map planning, intent, new objects, head publication, and awaited cleanup.
- **Recovery open**: recovery plus ordinary engine initialization after a fault.

Explicit checkpoint precedes the measured save, so its local I/O is observable
before save consumes the session. Save still performs its required empty-WAL
checkpoint internally. Local database and private-WAL bytes are reported separately
from durable KV traffic. Distinct-key counts are per phase; the analyzer never adds
them across phases because the same page can be read during open and query.

Stored value/key sizes are live logical bytes at phase end, not allocator usage,
memory high-water marks, physical disk occupancy, or garbage awaiting compaction.
All traffic measurements use healthy operations or a healthy recovery after fault
injection. The fault wrapper's event log, rather than MemoryKv's successful-operation
counters, records failed attempts. Visible-only fault hooks do not count as successful
puts/deletes and must not be used to infer failed-RPC traffic.

## Workloads

Every configuration uses an integer primary key, integer `bucket`, text `payload`,
and one secondary index on `bucket`. Cases are 100 rows × 64 B, 300 × 1 KiB,
100 × 9,000 B (overflow pages), and 10,000 × 256 B. All three existing map layouts
run against fresh byte-for-byte forks. There is no shared cache or compression.

Read workloads: PK hit and miss; ten-row PK range; a covering-index lookup returning
IDs; the same indexed predicate fetching payloads; an explicitly unindexed equality
filter; and a full payload scan. Bucket 3 selects 10% of rows spread throughout the
table. Full scans and filters use `NOT INDEXED`; index workloads use `INDEXED BY`.
Each query repeats in the same session for its warm observation. Returned row counts,
full payload values and covering-index IDs are checked. Query results are materialized
in memory by the existing Session API.

Write workloads: read-only save, SQL rollback, payload replacement, indexed-field
update, insert, delete, ten adjacent row replacements, and ten evenly spaced row
replacements. Ten-row updates share one transaction and one save. Recovery cases
fail before head publication, lose its durable acknowledgement, fail an old-page
delete, or fail intent deletion. A first HeadOverrides update can retain the old
base page; configurations with no block deletion explicitly skip that failpoint.

Every completed write is followed by simulated crash, fresh-engine SQL checks and
`PRAGMA integrity_check`. Verification traffic is outside the measurement. Recovery
cases check the selected old/new value, integrity, and intent removal. Aborts also
compare exact key sets and stored bytes with the original snapshot.

## Amplification denominators

There is no single useful amplification number. The summary CSV defines:

| Metric | Numerator | Denominator |
|---|---|---|
| Read amplification | Returned KV value bytes + get key bytes | Application payload bytes returned |
| Application write amplification | Put value bytes + put and delete key bytes | Logical replacement/insert bytes |
| Page-to-KV write amplification | All put value bytes | Immutable database page bytes put |
| Storage vs database | Live stored KV value bytes | Current database file bytes |
| Storage vs application | Live stored KV values + keys | Live application records, counting two 8-byte integers plus payload |

The CSV retains value-only and key-inclusive numerators, as well as operation
counts, so other backend cost models can be applied without rerunning SQL. Map,
head and intent bytes are separated. Read amplification includes open for cold
queries, and only the repeated query for warm observations.

Integers use a declared logical width of eight bytes, independent of SQLite varint
encoding. A payload replacement counts the entire supplied field, not only the
character positions that changed. Insert counts both integers plus the payload.
Covering-index results count eight bytes per returned ID. These are application
data conventions, not measured HTTP/SQL request lengths. Deletes report removed
application bytes separately; their write-amplification ratio is undefined, as
are read misses and no-op saves. Undefined ratios are blank, not zero or infinity.

Delete value size is **reclaimed bytes**, never outbound delete traffic. Wire
framing, response status bytes, backend write batches, tombstones, replication,
compression and RocksDB compaction are unmodeled. Multiplying these logical bytes
by a replica count is only a model, not a measurement of physical amplification.
Application write amplification excludes read-before-write traffic; that is
reported separately as read bytes and gets.

## Failpoints

`simulation::FaultKv` wraps MemoryKv with a reusable plan: operation, key prefix,
one-based matching ordinal, effect, and error versus cancellation. Effects are
absent, atomic visible-only, or completed/durable with an acknowledgement lost.
For reads, Before/After correspond to before or after the backend lookup; the
failed value is never delivered successfully to Turso. A seeded yield schedule
perturbs the completion order of concurrent futures without wall-clock sleeps.

`tests/failpoints.rs` exercises every observed get across open, point/range/covering
queries, full scans and updates. It checks session poisoning, zero publication,
and intact saved SQL after errors/cancellation. Separate campaigns exercise
recovery metadata reads, concurrent object failures with write concurrency 1/2/8,
and five successive cleanup interruptions before successful recovery. Existing
publication, recovery-mutation and checkpoint campaigns remain in the test suite.

This is a deterministic simulator. It does not prove all thread schedules, power
loss behavior, or settlement of a remote request after its local future is dropped.
The host's exclusive-ownership and request-settlement requirements are unchanged.

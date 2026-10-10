# Small-database benchmark — 2026-10-10

Crate 0.8.0, format v7, unmodified Turso 0.8.2. Results include the fixed pending-save
protocol and save-time retirement. All data comes from `results/small_databases.csv`.

## Method

Release/optimized Rust 1.99.0 on Linux x86_64 in a shared container, Intel Xeon
Platinum 8573C, 9 visible CPUs and an 8-CPU quota. Async MemoryKv yields at least
once per operation and simulates durable acknowledgments in memory. There is no
network, disk, replication, Restate service or artificial RTT in these timings.

57 configurations: three map layouts × (10/100/300 rows × 64/1024-byte payload ×
0/1/3 secondary indexes, plus a 10,000-row/256-byte/one-index control). Three samples
per operation produce **1,485 observations** across 495 groups. Counters agree
exactly within every three-sample group. Timings are descriptive, not a statistically
powered comparison; do not extrapolate remote throughput or choose a default from
millisecond differences on this simulator.

Each configuration starts from a populated, saved database. Each operation/sample
uses a fresh byte-for-byte fork; setup/fork time is excluded. Cold reads include
open, engine initialization and query. Warm reads repeat the same primary-key
query in the same session; there is no shared cache. Writes include open, SQL,
checkpoint, worker stop, intent/object/head writes and awaited cleanup.

Schema: integer primary key, three integer indexable columns a/b/c, an unindexed
integer note, and text payload. Secondary indexes cover a, or a/b/c. Payload is
zero-padded numeric text, with no compression. Updating a affects one secondary
index when present. Primary/secondary lookups fetch payload length, so they are not
merely covering-index queries. SQL delete does not promise to shrink the DB file.

After timing every write, the harness simulates a crash, opens a fresh engine,
checks row count and updated values, runs `PRAGMA integrity_check`, and verifies
absence of the pending record. These verification reads are excluded from counts.

Bytes are K/V **values**. Keys, request framing, delete traffic, backend metadata
and replication multiplication are excluded. A get count can include repeated
reads of the same page. Stored bytes are logical values, not physical disk usage.

## One non-indexed-field update

| Rows / payload / indexes | Layout | Gets | Puts | Deletes | Page bytes | Map-node bytes | Head bytes | Intent bytes | Total value bytes written |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 / 64 B / 1 | Radix | 10 | 4 | 3 | 4,096 | 27 | 34 | 124 | 4,281 |
| 10 / 64 B / 1 | InlineRoot | 9 | 3 | 2 | 4,096 | 0 | 53 | 144 | 4,293 |
| 10 / 64 B / 1 | HeadOverrides | 9 | 3 | 2 | 4,096 | 0 | 82 | 202 | 4,380 |
| 100 / 64 B / 1 | Radix | 11 | 4 | 3 | 4,096 | 45 | 34 | 124 | 4,299 |
| 100 / 64 B / 1 | InlineRoot | 10 | 3 | 2 | 4,096 | 0 | 71 | 180 | 4,347 |
| 100 / 64 B / 1 | HeadOverrides | 10 | 3 | 2 | 4,096 | 0 | 114 | 266 | 4,476 |
| 300 / 1024 B / 3 | Radix | 11 | 4 | 3 | 4,096 | 945 | 34 | 124 | 5,199 |
| 300 / 1024 B / 3 | InlineRoot | 10 | 3 | 2 | 4,096 | 0 | 971 | 1,980 | 7,047 |
| 300 / 1024 B / 3 | HeadOverrides | 11 | 3 | 1 | 4,096 | 0 | 50 | 113 | 4,259 |
| 10,000 / 256 B / 1 | Radix | 15 | 5 | 4 | 4,096 | 2,331 | 34 | 142 | 6,603 |
| 10,000 / 256 B / 1 | InlineRoot | 14 | 4 | 3 | 4,096 | 2,304 | 53 | 162 | 6,615 |
| 10,000 / 256 B / 1 | HeadOverrides | 15 | 3 | 1 | 4,096 | 0 | 50 | 113 | 4,259 |

For the 100-row, 64-byte payload, one-index default Radix case, one update writes
4096 payload bytes + 45 node bytes + 34 head bytes + **124 intent bytes** = **4299
value bytes**. Four puts are page, node, intent and head; three deletes are old
page, old node and intent. Stored values remain at 20,559 bytes after repeated saves.

InlineRoot saves one root put and one root delete for this small database, at the
cost of a larger head copied into the intent. With 300 wide rows its embedded root
is substantially larger: total write bytes are 7047 versus Radix's 5199. A larger
inline head is written in the intent as both old and new, then as the committed head.

HeadOverrides can defer base-map rewrites. After the 300-wide-row and 10,000-row
initial bulk saves, a first one-page update adds an override, retains the shadowed
base page, and deletes only the intent. Logical storage increases by 4112 bytes
(one 4096-byte page plus a 16-byte override). Subsequent updates replace that
same override and reclaim its preceding object, so repeated updates do not keep
growing storage. A fold later retires shadowed base pages. This is bounded
structural retention, not an untracked leak; at most 64 base pages are shadowed.

## Grouping updates

100 rows, 64-byte payloads, one secondary index. Ten updates target the same
unindexed note field. Each SQL statement commits locally in both cases.

| Layout | Save boundary | Puts | Deletes | Value bytes written |
|---|---|---:|---:|---:|
| Radix | One save | 4 | 3 | 4,299 |
| InlineRoot | One save | 3 | 2 | 4,347 |
| HeadOverrides | One save | 3 | 2 | 4,476 |
| Radix | Ten saves | 40 | 30 | 42,990 |
| InlineRoot | Ten saves | 30 | 20 | 43,470 |
| HeadOverrides | Ten saves | 30 | 20 | 44,760 |

Grouping ten provisional updates into one durable save reduces write traffic tenfold
in this workload. That is a change in acknowledgment boundaries: it is useful only
when the application can delay durability until the grouped save completes.

## Cold versus same-session warm lookup

100 rows, 64-byte payloads, one secondary index. Timings below are medians of three
samples from the in-memory release build; open/thread setup is included for cold.

| Layout | Cold PK gets | Cold PK bytes read | Cold PK median µs | Warm PK gets | Warm PK median µs |
|---|---:|---:|---:|---:|---:|
| Radix | 10 | 28,751 | 944 | 0 | 60 |
| InlineRoot | 9 | 28,743 | 924 | 0 | 60 |
| HeadOverrides | 9 | 28,786 | 1030 | 0 | 58 |

Every same-session warm lookup across all configurations issued zero K/V gets.
This measures the existing engine cache, not a new cross-session LRU. Cold opens
still fetch the pending key and head. Full CSV data also includes secondary-index
lookups, indexed-field updates, insert, delete, and larger-control timings.

## Reproduce and next measurement

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo run --locked --release --example small_databases -- 3 > results/small_databases.csv
```

Keep Radix as the existing default until testing the actual backend. InlineRoot's
reduced operation count is attractive for tiny databases, while its larger repeated
head affects replicated bytes. HeadOverrides trades a bounded amount of retained
storage and occasional folds for cheaper writes. The next useful benchmark adds
real durable RPC latency and replication, then measures limits on concurrent objects
and active sessions. Compression, integer codecs and cross-session caches remain
unimplemented; this run establishes their baseline.

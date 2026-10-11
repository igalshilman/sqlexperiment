# Owner cache: memory and disk, encoded or decoded, and engine reuse — 2026-10-11

`benches/cache_cpu.rs` measures a long-lived owner: 300 sessions over a
10,000-row table of JSON documents (half random hex; 493 pages, 2 MB
logical, about 0.8 MB encoded). Each session opens, runs four skewed point
lookups (90% within a hot tenth of the rows) and a 50-row range scan; every
tenth session also updates a row and saves. All sessions share one
`PageCache` whose memory is sized as a fraction of the logical database, with a
1 GiB disk tier behind it. The backend is strong, so nothing is confirmed. A
counting global allocator records allocations; time is the median of three
repeats on this shared host.

```sh
cargo bench --locked --features simulation --bench cache_cpu -- 3 > benches/results/cache-cpu.csv
```

## Memory and disk (foyer)

The cache is process-wide: pages live in memory and spill to a local disk tier
([foyer](https://github.com/foyer-rs/foyer) 0.22.3, single-file device, 1 MiB
blocks), shared by every namespace. Engine reuse is on.

| Cache | Memory (of logical DB) | Page gets | Round trips | Allocated MB | CPU ms |
|---|---|---|---|---|---|
| encoded | 5% | 239 | 503 | 82.5 | 148 |
| decoded | 5% | 239 | 503 | 82.6 | 144 |
| encoded | 10% | 239 | 503 | 77.7 | 110 |
| decoded | 10% | 239 | 503 | 76.8 | 111 |
| encoded | 25% | 239 | 503 | 74.1 | 67 |
| decoded | 25% | 239 | 503 | 69.5 | 69 |
| encoded | 50% | 239 | 503 | 72.5 | 62 |
| decoded | 50% | 239 | 503 | 67.5 | 52 |
| encoded | 100% | 239 | 503 | 72.3 | 58 |
| decoded | 100% | 239 | 503 | 66.6 | 50 |

- **No backend misses beyond first touches at any memory size.** The in-memory
  LRU (below) needed 1,120 misses and 1,450 round trips at 5%; the disk tier
  serves them locally. Sequential readahead turns some first touches into
  pages fetched ahead: 239 gets in 503 round trips. Each avoided round trip is worth milliseconds on a real
  backend. At 5% memory a session takes about 0.5 ms of host time, mostly local
  disk reads.
- **foyer costs about 0.05 ms per session** once the working set fits in memory:
  48–75 ms (noisy on this shared host) against 36–43 ms for the
  hand-written LRU. Allocated bytes fell
  about 15%.
- **Encoded or decoded matters less now:** a miss in memory is a local read,
  not a round trip. Decoded saves a little CPU and allocation once it fits.

The CPU column is wall time on a shared host and includes waiting for the disk
tier's thread. The in-memory LRU results are in
`results/cache-cpu-memory-lru.csv`; the sections below describe that design.

## Engine reuse (in-memory LRU)

With a `PageCache`, a session that ends cleanly (a successful save, or a
read-only session) leaves its idle engine in the cache. The next open reuses it
when nothing changed since: the cache validated `meta` and the log ends where
that engine left it. Its parsed schema and engine page cache stay warm. Opening
still reads `meta` and probes the log, so round trips do not change.

| 300 sessions, encoded cache | CPU ms | Allocations | Allocated MB |
|---|---|---|---|
| Fresh engine per session | 221–232 | 965k–972k | 169–172 |
| Reused engine | 39–43 | 205k–213k | 86–89 |

CPU per session drops from about 0.74 ms to 0.14 ms (5.4×), and allocations
drop 4.7×. Another writer's append or checkpoint, unsaved changes, a query error
or a failed save each force a fresh engine. Tests cover each case. The engine
also validates its own page cache against the database header at every read
transaction, so a stale reuse would not return stale rows; the explicit check
is the primary guard.

## In-memory LRU with engine reuse (300 sessions)

| Cache | Size (of logical DB) | Page misses | Round trips | Allocated MB | CPU ms |
|---|---|---|---|---|---|
| encoded | 5% | 1,120 | 1,450 | 88.7 | 43 |
| decoded | 5% | 1,243 | 1,573 | 87.7 | 39 |
| encoded | 10% | 375 | 705 | 86.2 | 42 |
| decoded | 10% | 706 | 1,036 | 83.7 | 41 |
| encoded | 25% | 207 | 537 | 85.7 | 41 |
| decoded | 25% | 222 | 552 | 80.1 | 38 |
| encoded | 50% | 179 | 509 | 85.6 | 39 |
| decoded | 50% | 180 | 510 | 79.8 | 36 |
| encoded | 100% | 179 | 509 | 85.6 | 43 |
| decoded | 100% | 179 | 509 | 79.8 | 39 |

With the engine warm, decoding is a visible share of what remains: decoded
caching saves about 7% CPU and 7% allocated bytes once the working set fits,
but still costs 11–88% more misses when it does not.

## In-memory LRU without engine reuse (300 sessions)

| Cache | Size (of logical DB) | Page misses | Round trips | Allocated MB | CPU ms |
|---|---|---|---|---|---|
| encoded | 5% | 1,123 | 1,453 | 172.3 | 221 |
| decoded | 5% | 1,390 | 1,720 | 168.0 | 222 |
| encoded | 10% | 362 | 692 | 169.8 | 228 |
| decoded | 10% | 670 | 1,000 | 162.7 | 213 |
| encoded | 25% | 207 | 537 | 169.3 | 232 |
| decoded | 25% | 222 | 552 | 159.4 | 216 |
| encoded | 50% | 179 | 509 | 169.2 | 228 |
| decoded | 50% | 180 | 510 | 159.1 | 220 |
| encoded | 100% | 179 | 509 | 169.2 | 221 |
| decoded | 100% | 179 | 509 | 159.1 | 219 |

The remaining 179 misses are first touches of cold pages, which no cache avoids.

## Findings (in-memory LRU)

- **Encoded is the right default.** The same bytes hold about 2.5× more pages.
  When the working set does not fit decoded (5–10% here), decoding costs
  19–85% more misses and up to 45% more round trips, each worth milliseconds
  on a real backend.
- **Decoding saves little CPU.** Once both fit, decoded caching saves at most
  about 5% of CPU time, which is within this host's noise, and about 6% of
  allocated bytes. LZ4 decoding is a few microseconds per session.
- **The engine dominated CPU.** Without reuse, about 0.7 ms and 3,200
  allocations per session came from opening a fresh engine (schema parsing,
  statement preparation). Engine reuse removes most of it (above).
- **Copy cleanup.** Sharing pages instead of copying them cut allocated bytes by
  about 11% (189–195 MB to 169–172 MB) with the same cache. Allocation counts and
  CPU time did not change measurably. The previous numbers are in
  `results/cache-cpu-before-cleanup.csv`.

`PageCacheConfig::decoded` stays available for owners with memory to spare.
Raw results: `results/cache-cpu.csv` (memory and disk),
`results/cache-cpu-memory-lru.csv` (in-memory LRU with reuse), `results/cache-cpu-no-reuse.csv`
and `results/cache-cpu-before-cleanup.csv`.

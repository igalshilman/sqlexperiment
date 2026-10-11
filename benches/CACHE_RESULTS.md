# Owner cache: encoded or decoded? — 2026-10-11

`benches/cache_cpu.rs` measures a long-lived owner: 300 sessions over a
10,000-row table of JSON documents (half random hex; 493 pages, 2 MB
logical, about 0.8 MB encoded). Each session opens, runs four skewed point
lookups (90% within a hot tenth of the rows) and a 50-row range scan; every
tenth session also updates a row and saves. All sessions share one
`PageCache`, sized as a fraction of the logical database. The backend is
strong, so nothing is confirmed. A counting global allocator records
allocations; time is the median of three repeats on this shared host.

```sh
cargo bench --locked --features simulation --bench cache_cpu -- 3 > benches/results/cache-cpu.csv
```

## Results (300 sessions)

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

## Findings

- **Encoded is the right default.** The same bytes hold about 2.5× more pages.
  When the working set does not fit decoded (5–10% here), decoding costs
  19–85% more misses and up to 45% more round trips, each worth milliseconds
  on a real backend.
- **Decoding saves little CPU.** Once both fit, decoded caching saves at most
  about 5% of CPU time, which is within this host's noise, and about 6% of
  allocated bytes. LZ4 decoding is a few microseconds per session.
- **The engine dominates CPU.** About 0.7 ms and 3,200 allocations per session
  come mostly from opening a fresh engine (schema parsing, statement
  preparation), not from storage.
- **Copy cleanup.** Sharing pages instead of copying them cut allocated bytes by
  about 11% (189–195 MB to 169–172 MB) with the same cache. Allocation counts and
  CPU time did not change measurably. The previous numbers are in
  `results/cache-cpu-before-cleanup.csv`.

`PageCache::decoded` stays available for owners with memory to spare.

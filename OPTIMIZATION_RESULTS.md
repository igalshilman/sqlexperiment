# Reconstructed cache and map optimizations — 2026-10-10

Crate **0.10.0**, experimental storage format **v8**, unmodified Turso **0.8.2**.
This is a fresh implementation from the saved 0.9.0/v7 checkpoint. The earlier
unarchived optimization source was unavailable; its reported numbers are not
used as evidence for this implementation.

## Changes

- `src/snapshot.rs` retains one validated immutable 4096-byte page per snapshot,
  keyed by object ID. This is an LRU with capacity one, targeting repeated header
  reads during Turso initialization. Dirty overlays/tombstones take precedence;
  WAL reads bypass it. Failed or malformed reads are not cached. New sessions and
  recovery never inherit cache entries, so aborted object-ID reuse cannot hit an
  earlier session's cached page. Locks are released before backend awaits.
- `src/map_codec.rs` chooses the smaller of fixed-width entries or an adaptive
  bitmap/sparse-edge representation with unsigned LEB128 offsets from the minimum
  ID. Full u64 allocation semantics remain unchanged. IDs need not be consecutive
  or increasing in logical page order. A dense node with IDs 1–256 occupies 420
  bytes, versus 2304 in v7. The worst encoded node is 2305 bytes including the tag.
- Head, object and pending-record keys/magic are versioned to v8. A v7 head or
  pending record in a namespace without a v8 head is rejected before mutations.
  Use a fresh dedicated namespace; no migration or backward compatibility is
  implemented. The pending-save/atomic-head publication protocol is unchanged.

## Executed validation

Rust/Cargo **1.90.0**, Linux x86_64; locked dependencies.

- Recovered baseline: **36 tests passed**, **4155 interruption cases**.
- Reconstructed implementation: **43 tests passed**, **4107 interruption cases**.
- `cargo fmt --check` and `cargo clippy --locked --all-targets -- -D warnings` pass.
- Both versions ran the release amplification benchmark with **3 repeats**:
  **309 workload/cache configurations**, **2043 phases**, **12258 class/phase rows**
  per version. The analyzer verified exact repeat agreement and reconciled
  per-object-class totals in each run.
- All 309 configurations have no increase in gets, puts, deletes, logical read
  traffic, or logical mutation traffic. This describes this workload matrix,
  not an assertion for all possible ID distributions or schemas.

Seven new tests cover codec shapes and the u64 range, malformed encodings and
overflow, 2000 seeded randomized codec cases, cache hits/eviction/session
isolation, uncached failed reads, v7 rejection, and overlay/WAL/truncate/regrow
behavior. Existing crash/reopen, bounded concurrent writes, folds, orphan
reclamation, and repeated recovery interruption tests run with the new codec.

The read-failure campaign falls from 1196 to **1148** cases because the cache
eliminates four gets in the dedicated open trace: 3 layouts × 4 gets × 4
error/cancellation positions = **48** cases. The five SQL traces arm their faults
after open, so their initialization reads were never counted in those traces.
Every observed get in each selected trace remains covered.
The unchanged campaigns contribute 2800 + 81 + 48 + 30 cases, totaling 4107.

## Measured costs

Radix, 10,000 rows, 256-byte payload. Cold-read totals include engine open.
Read traffic includes KV key and returned value bytes. Put values exclude keys.

| Measurement | v7 baseline | v8 reconstructed | Reduction |
|---|---:|---:|---:|
| Cold primary-key lookup gets | 14 | 10 | 28.6% |
| Cold lookup read traffic | 39,411 B | 17,765 B | 54.9% |
| One-page replacement map put values | 2,331 B | 433 B | 81.4% |
| One-page replacement total put values | 6,603 B | 4,705 B | 28.7% |
| One-page replacement mutation traffic including keys | 6,786 B | 4,888 B | 28.0% |

Cold open now fetches the header page once instead of five times. It saves 16 KiB
of repeated page data plus four key transmissions; map encoding reduces the
remaining metadata bytes. API call count for object publication is unchanged.

The smaller 100-row/64-byte payload replacement moves from 4434 to **4403 bytes**
of mutation traffic: **68.8×** the application payload. A full 4 KiB dirty page
still dominates that amplification. Page compression/sub-page storage is not
implemented. Scattered non-covering index queries also remain expensive; this
one-page cache does not turn them into covering-index queries.

## Reproduction and evidence

```sh
CARGO_INCREMENTAL=0 cargo test --locked -- --nocapture
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo run --locked --release --example amplification -- 3 > results/amplification.csv
python3 scripts/analyze_amplification.py
python3 scripts/compare_optimizations.py
```

`results/tests-optimization.log`, `results/baseline-tests.log`,
`results/clippy-optimization.log`, and both benchmark logs preserve this run.
`results/baseline-amplification.csv` and its summary contain the fresh v7 run;
`results/amplification.csv` and its summary contain v8. The comparison script
writes `results/optimization-comparison.csv`. Earlier small-database reports and
test logs remain historical; they were not relabeled as v8 results.

## Practical limits

These are logical KV costs from the in-memory visibility/durability simulator.
No remote latency, replication, wall-clock speedup, process kill or power-loss
claim follows. The cache bounds only one retained page payload per snapshot;
decoded nodes, private WAL, dirty pages, write plans and result rows remain
unbounded. The host still must provide exclusive ownership and settle cancelled
requests. No Restate backend or distributed fencing has been added.

# Validation — failpoints and amplification, 2026-10-10

## Current 0.9.0 run

Storage format remains v7 and Turso remains pinned to unmodified 0.8.2. The current
run uses Rust/Cargo 1.90.0 on Linux x86_64. `cargo fmt --check` and all-target Clippy
with `-D warnings` pass. The complete suite passes **36 tests**; output is in
`results/tests-amplification.log`, with Clippy output in
`results/clippy-amplification.log`.

The original 2,800 interruption cases reran successfully. New campaigns add:

| Campaign | Cases |
|---|---:|
| Every observed read: open, PK, range, covering index, full scan, update; three layouts; error/cancel before/after backend lookup | 1,196 |
| Object errors with concurrency 1/2/8, three seeded completion schedules, three atomic outcomes, three layouts | 81 |
| Pending/head reads during abort and commit recovery, before/after lookup and error/cancel | 48 |
| Five successive cleanup interruptions before final successful recovery, both outcomes and three layouts | 30 |
| **New cases** | **1,355** |
| **Combined interruption cases** | **4,155** |

The read campaigns verify poisoned sessions, zero backend mutations, settled
simulator requests and intact saved SQL. Concurrent-error tests check that all
requests settle, concurrency stays bounded and no head is published. Recovery
read failures prevent mutations; repeated cleanup failures retain the intent until
cleanup succeeds. A separate counter regression checks misses, repeated reads,
idempotent deletes, exact key bytes, reclaimed bytes and phase resets.

The extended benchmark is described in `AMPLIFICATION.md`; raw measurements and
generated aggregate results are separate from the historical benchmark below.
The release run covers **309 workload/cache configurations**, each repeated three
times: **2,043 phases** and **12,258 class/phase CSV rows**. Every counter agrees
across repeats and every object-class total reconciles. Warm queries issued zero
gets in all 84 workload configurations. Three HeadOverrides configurations omit a
block-delete fault that is unreachable because a shadowed base page is retained.
Logical counters do not establish latency, real transport cancellation behavior,
physical replication amplification or power-loss safety.

## Historical 0.8.0 run

Crate 0.8.0, format v7; Linux x86_64, Rust/Cargo 1.99.0, Turso 0.8.2 unmodified.

## Executed checks

- `cargo fmt --check`: passed.
- `CARGO_INCREMENTAL=0 cargo clippy --locked --all-targets -- -D warnings`: passed.
- `CARGO_INCREMENTAL=0 cargo test --locked -- --nocapture`: **32 tests passed**.
  Full output: `results/tests.log`.
- Release `examples/small_databases.rs`: **1,485 successful observations**, 57
  configurations, three repeats; counters agree within each group. Each measured
  write survives simulated crash and fresh-engine SQL/integrity verification.
  Raw CSV: `results/small_databases.csv`; interpretation: `BENCHMARKS.md`.

The existing large lazy-lookup example still builds under all-targets Clippy. Its
old phase-seven numbers are not presented as a new run; the new benchmark includes
a 10,000-row larger control and the integration suite tests a 4000-row lazy lookup.

## Interruption campaigns

| Campaign | Cases |
|---|---:|
| SQL publication, Radix: automatic checkpoint / explicit checkpoint / first save | 528 |
| SQL publication, InlineRoot: same three variants | 468 |
| SQL publication, HeadOverrides: same three variants | 468 |
| Recovery itself: three layouts, empty/existing database, abort/uncertain/committed saves | 1236 |
| Override fold when adding the 65th entry | 72 |
| Private checkpoint interruptions | 28 |
| **Total** | **2800** |

Publication/recovery/fold campaigns inject an error or cancellation with no effect,
atomic visible-only effect, or durable effect, followed by a crash that loses or
persists unacknowledged effects: 12 combinations per mutation. Every acknowledged
mutation survives. Checkpoint tests separately cover before/after local effects.
The fold-specific campaign injects every put; small-save campaigns inject both
puts and deletes. Recovery cases interrupt recovery's head confirmation, object
deletes and intent deletion. This is not exhaustive over concurrent schedules.

Recovery verifies SQL values, multi-page payloads, fresh-engine integrity checks,
and a later successful save. Exact key-set and stored-byte comparisons against
clean reference snapshots check that both aborted new writes and committed old
objects are reclaimed, without scanning in the adapter. First-save recovery also
covers confirming an absent head. Failure of head confirmation causes no subsequent
mutation. Another test replaces an uncertain intent deletion with a new save before
any crash; the old durable record cannot resurrect after a successful new save.

## Mapping and metadata

A deterministic sequence of 150 sparse rewrites per layout (450 total), plus final
emptying, checks all page values and exact structural object accounting after each
save. It exercises growth/collapse, deletion/regrowth, shared subtrees and override
folds. Test-only traversal intentionally includes structurally retained shadowed
base pages. Other tests cover index 2^40, 64 overrides, the 65th-entry fold, tombstones,
ID reuse only after recovery, and u64 exhaustion before backend mutations.

Malformed/truncated intent records, invalid ID ranges, invalid object kinds,
unexpected heads, referenced-object absence and invalid map/block lengths are
rejected. An oversized intent explicitly fails encoding at 1 MiB. This is malformed
input coverage, not a proof against arbitrary valid-looking corruption.

The 16 MiB block-level comparison remains in `results/tests.log`. Representative
single-block changes: Radix 5 puts / 2624 metadata bytes; InlineRoot 4 puts / 2870
metadata bytes; HeadOverrides 3 puts / 163 metadata bytes (base page retained until
fold). Cold changed-block lookup takes respectively 5, 4, 3 gets including the
pending-key check. These are value/operation counts, not remote latency claims.

## Adapter and engine lifecycle

Reordering object puts reach exactly eight outstanding calls in the dedicated
backend test, complete out of order, and drain on ordinary errors before any head
write. Session callbacks now run on the engine worker through IO::step. A regression
checks callback thread identity, queued-completion in-flight counting, and aborting
ready completions on shutdown. Concurrent-driver serialization and in-flight-count
regressions from the earlier self-review remain passing.

Other tests cover private WAL isolation, refusing open SQL transactions/nonempty
WAL publication, rollback/savepoints, local commit coalescing, partial and vectored
I/O, short reads, truncate/regrow, backend read errors and query cancellation.
The 100-local-UPDATE fixture writes 428,512 private WAL bytes but saves just two
final 4 KiB blocks and 133 metadata bytes: **8325 backend value bytes**.

## Limits

MemoryKv models visibility and durability in process memory. No real backend,
Restate integration, power loss, process kill, distributed failover or network
benchmark was run. Tests assume ownership and request settlement, rather than
implementing them. No payload checksum, corruption repair, old-format migration,
chunked intent, cross-session cache or resource-budget implementation is claimed.

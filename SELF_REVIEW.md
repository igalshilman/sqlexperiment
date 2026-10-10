# Author self-review — 2026-10-10

This section preserves the historical 0.9.0 review. The reconstructed 0.10.0
changes and their validation are documented in `OPTIMIZATION_RESULTS.md`.

Scope: fixed-key pending-save protocol, retirement planning, recovery, engine I/O,
36 passing tests, and the small-database/amplification benchmarks. This is an author review,
not an independent production audit. Compression/cache experiments remain deferred.

## Changes and findings

The earlier retirement-only direction covered successful saves but left abandoned
new objects for full-store GC. The user's write-ahead intent proposal covers both
sets in one record. `src/pending.rs` now owns that protocol. `src/gc.rs`, `KvScan`
and GC tests were replaced by automatic recovery and its failure campaigns.

The head remains the only commit decision. The intent is durable before object
writes. Recovery accepts only exact old/new head matches and confirms its selected
head durably before destructive cleanup. Acknowledged object deletes precede intent
deletion. A failed intent deletion is safe to retry because both cleanup branches
are idempotent. First-save rollback uses durable head deletion to confirm absence.
Ordinary errors drain outstanding operations; cancellation still requires host
settlement. Committed cleanup failure is represented by `Ok(SaveReport)` with
`cleanup_error`, so callers need not mistake it for a rolled-back SQL operation.

Retirement is collected during changed-path rewrites, not by diffing whole maps.
Final-plan filtering preserves reused old roots/subtrees and prunes transient new
wrappers before backend writes. Exact-key tests cover 450 sparse rewrites plus
emptying. HeadOverrides deliberately retains up to 64 shadowed base pages until
fold; repeated replacement of the same override reclaims the previous override.

## Engine completion race found during this phase

Concurrent SQL fault campaigns intermittently returned Turso's generic
`CompletionError(IOError(Other, "write"))` during otherwise healthy local commits.
Inspection of pinned upstream `io/completions.rs` showed group completion decrements
its outstanding count to zero before setting its successful result. The pager's
`CommitState::WaitWrites` checks finished and then succeeded. Running callbacks on
the async host while the engine polls on another thread can expose that intermediate
state. The campaigns failed before the adapter change and passed afterward.

The adapter now queues successful completions for delivery from `IO::step` on the
engine worker. Polling and successful group finalization therefore run on the same
thread. No upstream patch is used. Queued completions count as in flight; shutdown
aborts undelivered ones. Cancellation/error completions preserve existing abort
behavior. Standalone public KvIo::drive behavior stays immediate by default; Session
selects worker delivery before spawning its engine. A targeted test checks worker
thread identity and shutdown behavior. See `src/io.rs` and `tests/publication.rs`.

## Earlier reproduced fixes retained

- A public low-level drive caller could overtake a suspended earlier write.
  An async driver mutex preserves submission order across awaits. The regression
  previously failed with byte 1 instead of byte 2.
- pending_count previously counted only queued operations, omitting dequeued work.
  It now counts all in-flight requests, including ready worker callbacks. The
  original regression previously observed 0 instead of 1.

## Evidence

The current 0.9.0 run is in `VALIDATION.md` and `results/tests-amplification.log`:
36 tests and 4155 interruption/recovery cases. The original 0.8.0 run remains in
`results/tests.log`: 32 tests and 2800 interruption/recovery cases.
Formatting and all-target Clippy with warnings denied pass. `BENCHMARKS.md` records
1485 release observations and their assumptions. The default remains Radix; no
universal layout winner is inferred from in-memory timings. The new intent copies
both head values, making large InlineRoot metadata more expensive than before.

The 0.9.0 additions instrument the simulator and introduce reusable failpoints;
they do not change storage format or publication behavior. The test expectation
for a sample block-key length was corrected from 24 to 25 bytes during development.
No new storage correctness defect was reproduced. The benchmark analyzer checks
class totals and repeat agreement, keeps misses/no-op denominators undefined,
does not sum per-phase unique page counts, and distinguishes reclaimed bytes from
delete traffic. `AMPLIFICATION.md` defines all denominators and exclusions.

## Remaining limits

- The host must enforce one owner, exclude old readers and settle cancelled remote
  requests. The adapter does not supply distributed fencing or handle late writes.
- Query execution is not a SQL sandbox; long CPU-only engine work is not forcibly
  preempted. One worker per session and unbounded result collection remain.
- A 1 MiB intent limit is explicit, but the private WAL, dirty overlay, object plan
  and caches still have no byte budgets. Backend maximum value size must support it.
- Open blocks new work until pending cleanup completes; persistent delete failures
  therefore affect availability even when the chosen database is already durable.
- No payload checksum, arbitrary-corruption repair, old-format migration, attached
  database guarantee, real remote backend or process-kill/power-loss validation.
- Malformed metadata is rejected; syntactically valid but incorrect retirement
  metadata is not protected by a full live-map validation or a checksum.
- Existing v6 leaked objects are outside v7 recovery. Use a fresh namespace.

## Publishing

Target: https://github.com/igalshilman/sqlexperiment, branch `main`.
Source, tests, design notes and raw benchmark results are included. Build
artifacts and distribution archives are excluded.

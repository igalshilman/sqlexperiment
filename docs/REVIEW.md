# Review and validation — 2026-10-10

The adapter now uses one page-map implementation: an adaptive-height, 256-way
radix tree with its root embedded in the snapshot head. Save phases are ordered;
mutations within a phase run concurrently. An optional object cache is shared
across sessions. An optional durable delta log commits a save with one put. The
caller drives the engine directly, without a worker thread. This is an author
review supported by executed tests, not an independent audit.

## Durable delta log (format v11)

- `StorageOptions::max_log_segments` enables appending a save's changed pages
  as one segment, a single put that commits it. Every `max + 1` saves, or for an
  oversized segment, the save folds the log into the map through the intent
  protocol, retiring the segments. Policy zero, the default, publishes every
  save as before and folds any existing log.
- The head gained `log_start` (zero disables probing), giving format v11
  (`SK11`/`SP11`/`SL11`). The intent can retire log segments (kind 2), validated
  against the old and new log starts.
- Open re-puts the last segment before exposing pages, so a later save never
  builds on a segment that is visible but not durable. A campaign showed that
  removing this step breaks the crash checks. With an object cache, the first
  probe batch shares the head's round trip; stale hints are discarded.
- `simulation::LatencyKv` measures critical-path round trips on a virtual clock.
  `benches/log_shipping.rs` compares the policies with and without a cache; see
  [LOG_RESULTS.md](../benches/LOG_RESULTS.md). In short: saves go from 5–7 RT to
  1 RT for appends, write sessions get 9–45% faster, opens cost about 1 RT more,
  folds form the save tail, and roughly twice the bytes are written.
- Tests cover the segment codec and malformed segments, append/fold accounting,
  policy switches, oversized segments, confirmation of visible segments, log
  gaps, stale cache hints, SQL sessions across 15 appends and 4 folds, and
  60 interrupted append/fold cases with and without crashes. Every case checks
  atomicity, the next save and exact orphan accounting.

## Concurrent phases and object cache

- Object puts, retirement deletes and recovery deletes run concurrently within
  their phase, at most 16 at a time, so latency grows with ⌈objects/16⌉ instead
  of the object count. Phases stay ordered. After the first error no mutation
  starts and running ones are drained; same-key requests never overlap.
- `ObjectCache` is an optional byte-bounded LRU of encoded committed objects,
  shared by successive sessions of one namespace. It caches values read through
  a recovered head and a save's objects once its head put is acknowledged, and
  never objects of an unacknowledged attempt, whose IDs recovery may reuse. A
  test reuses such IDs from a session without the cache and fails if
  uncommitted objects are cached.
- Code cleanup: typed object kinds replace strings, the snapshot keeps its
  decoded `Head`, mutations are plain data applied by one function (keeping
  futures `Send`), block-splitting loops and yield helpers are shared, and
  simulator counters derivable from per-kind traffic were removed.

## Map decision

The original layout options supported comparison experiments. Those experiments
are complete; callers no longer select an internal representation. The embedded
root saves a dependent map get on cold reads and avoids writing and retiring a
separate root object during updates. It retains immediate retirement of replaced
pages and nodes, and reuses unchanged subtrees.

This does not minimize every byte count. A large embedded root is included in
both the head and pending intent, so it can cost more write bytes than an external
root. The choice favors fewer requests and one simple, predictable lifecycle.
No real-backend latency improvement is claimed by these simulator measurements.

## Changes

- Removed the public map enum, options field, layout dispatch and CLI selector.
- Removed external-root publication, head overrides, tombstones and deferred folds.
  Deleting a page removes its tree edge during the same save.
- Simplified the head to a fixed prefix followed by one encoded root. There are
  no layout tags, root-length fields, override counts or alternate decoders.
- There is a single current format (now v11). Use a fresh namespace; no
  migration or compatibility code is included.
- Reduced benchmark and test matrices to the fixed map. The interrupted-fold
  campaign now tests ordinary tree rewrites, including abandoned writes followed
  by a different retry. Sparse growth, collapse and exact reclamation remain tested.
- Added head validation coverage for truncation, malformed roots, inconsistent
  heights, trailing data and out-of-range object IDs.
- Removed the write-concurrency option. The complete intent permits recovery to
  delete even planned objects that were never written.
- Read head and intent together during quiescent open. The engine I/O pump runs
  adjacent queued reads in batches of at most 16; writes, truncation and sync are
  ordering barriers. Dependent map traversal within one read remains sequential.
- Added gated-backend tests for overlapping reads, barrier ordering, draining
  after an ordinary error and retiring completions on cancellation, including a
  dequeued future that is never polled. Phase tests cover bounded overlap,
  phase ordering, draining after an object error, aborted allocation cleanup
  and retirement with exact orphan accounting.
- Added module/type/method rustdoc covering binary layouts, overlay state, caches,
  allocation and retirement, publication durability, uncertain outcomes and host
  ownership. The crate overview explains immutable pages plus a radix map against
  in-place updates, a whole-database value and a durable engine WAL.
- Regenerated all current benchmark evidence and removed the layout dimension
  from CSVs, analyzers, examples and documentation.

## Host-driven execution

`Session` owns the engine connection directly. Open/connect resume their engine
state machines after awaiting queued KV work. Queries step the engine, drive
reported I/O asynchronously and resume; checkpoint follows the same path. Save
drops the checkpointed connection before publication. The command/result/event
channels, worker thread, condition variable and worker-ready queue are removed.

The host runs completion callbacks. A statement cancellation guard aborts queued
requests before statement destruction, and the session remains poisoned. Cached
queries return to the executor after 64 steps; a single engine call is not
preemptible. The open future remains `Send`: the engine's owned schema guard uses
`parking_lot`'s supported `send_guard` feature, enabled in Cargo without adding a
new resolved crate or changing the engine source.

The pinned engine still calls synchronous metadata helpers. Open asynchronously
loads page zero and retains it; dirty overlay entries override that original
header. Synchronous `IO::step` serves resident reads and private sync barriers
only. Nonresident reads, file mutations or waits without queued work fail
explicitly, without polling KV or blocking the host. Such a synchronous engine
path requires a nonblocking upstream implementation to become supported; it is
not retried by replaying SQL. An error poisons the session.

Regression checks cover host-side callbacks, cached-query scheduling and
cancellation, `Send` open futures, multi-page cold schemas, ANALYZE statistics,
cache eviction, schema changes and synchronous-I/O rejection. Header cache tests
verify that overlay writes, WAL bytes and truncation retain precedence.

## Boundaries

The default library contains the KV contract, engine adapter, codecs and one
save/recovery protocol. In-memory durability simulation, failpoints, counters and
artificial yields require `simulation`. Test bodies and helpers are under `tests/`;
checkpoint injection is test-only. Benchmarks, analyzers and results are under
`benches/`, with explicit Cargo bench targets separate from normal test commands.

## Executed validation

Rust/Cargo 1.97.0, locked dependencies, Linux x86_64.

| Check | Outcome |
|---|---|
| `cargo test --locked --no-default-features` | 12 tests and one compile-checked rustdoc example pass |
| `cargo test --locked --features simulation` | 69 tests pass: 45 unit, 23 integration, one public API; one rustdoc example also passes |
| Interruption campaigns | 1355 publication cases plus 60 delta-log cases |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Pass |
| `cargo fmt --check` | Pass |
| `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features --document-private-items` | Pass |
| Amplification benchmark, three repeats | 4104 observations / 684 phases / 104 configurations; counters reconcile |
| Page-compression benchmark, three repeats | 504 SQL observations / 84 Raw-LZ4 pairs / 72 codec timing samples; traffic agrees |
| Small-database benchmark, three repeats | 495 observations; SQL/reopen checks pass |
| Delta-log benchmark, two repeats | 12 configurations × 128 sessions; repeats identical; final reads verify every write |
| All three benchmark analyzers | Pass on v11 evidence |

The interruption total includes 28 checkpoint, 48 tree-rewrite, 380 read,
9 object-error, 16 recovery-read, 10 repeated-cleanup, 468 publication
and 396 interrupted-recovery cases. The object-error campaign checks that a failed
phase drains and that object puts overlap.

## Remaining limits

Exclusive ownership and settlement of outstanding requests remain host
requirements. Memory budgets, request fencing, real-backend latency and process
crash/power-loss validation remain future work. No storage correctness defect was
reproduced in the supported map by this review.

See [format](FORMAT.md), [protocol](PROTOCOL.md), and
[benchmark methodology and evidence](../benches/README.md).

# Review and validation — 2026-10-10

The adapter now uses one page-map implementation: an adaptive-height, 256-way
radix tree with its root embedded in the snapshot head. Backend mutations run one
at a time; independent reads can overlap. The caller drives the engine directly,
without a worker thread. This is an author review supported by
executed tests, not an independent audit.

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
- Simplified the head to a 21-byte prefix followed by one encoded root. There are
  no layout tags, root-length fields, override counts or alternate decoders.
- The single current format is v10 (`SK10`/`SP10`). Use a fresh namespace; no
  migration or compatibility code is included.
- Reduced benchmark and test matrices to the fixed map. The interrupted-fold
  campaign now tests ordinary tree rewrites, including abandoned writes followed
  by a different retry. Sparse growth, collapse and exact reclamation remain tested.
- Added head validation coverage for truncation, malformed roots, inconsistent
  heights, trailing data and out-of-range object IDs.
- Removed the write-concurrency option. Intent, object and head puts and all
  cleanup/recovery deletes are serial, stopping at the first error. The complete
  intent permits recovery to delete even planned objects that were never written.
- Read head and intent together during quiescent open. The engine I/O pump runs
  adjacent queued reads in batches of at most 16; writes, truncation and sync are
  ordering barriers. Dependent map traversal within one read remains sequential.
- Added gated-backend tests for overlapping reads, barrier ordering, draining
  after an ordinary error and retiring completions on cancellation, including a
  dequeued future that is never polled. Serial-mutation tests cover both aborted
  allocation cleanup and successful retirement with exact orphan accounting.
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

Rust/Cargo 1.90.0, locked dependencies, Linux x86_64.

| Check | Outcome |
|---|---|
| `cargo test --locked --no-default-features` | 8 tests and one compile-checked rustdoc example pass |
| `cargo test --locked --features simulation` | 54 tests pass: 33 unit, 20 integration, one public API; one rustdoc example also passes |
| Interruption campaigns | 1355 cases across the one supported map |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Pass |
| `cargo fmt --check` | Pass |
| `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features --document-private-items` | Pass |
| Amplification benchmark, three repeats | 4104 observations / 684 phases / 104 configurations; counters reconcile |
| Page-compression benchmark, three repeats | 504 SQL observations / 84 Raw-LZ4 pairs / 72 codec timing samples; traffic agrees |
| Small-database benchmark, three repeats | 495 observations; SQL/reopen checks pass |
| Both benchmark analyzers | Pass on current-format evidence |

The interruption total includes 28 checkpoint, 48 tree-rewrite, 380 read,
9 serial-object-error, 16 recovery-read, 10 repeated-cleanup, 468 publication
and 396 interrupted-recovery cases. The former write-concurrency settings no
longer multiply the same object-error campaign; the only supported value is one.

## Remaining limits

Exclusive ownership and settlement of outstanding requests remain host
requirements. Memory budgets, request fencing, real-backend latency and process
crash/power-loss validation remain future work. No storage correctness defect was
reproduced in the supported map by this review.

See [format](FORMAT.md), [protocol](PROTOCOL.md), and
[benchmark methodology and evidence](../benches/README.md).

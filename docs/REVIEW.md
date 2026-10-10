# Review and validation — 2026-10-10

Storage format v12 replaces the radix page map, snapshot head and intent record
of v11 with a durable log, pages checkpointed in place and a small checkpoint
record. This is an author review supported by executed tests, not an
independent audit.

## Design decision

v11 committed a save by writing new immutable page objects and switching one
head to a new map. Every structure existed to make one put an atomic multi-page
commit: the map (stable page numbers to fresh object IDs), the head (one switch),
the intent (reclaim aborted or replaced objects) and the allocation counter.

A durable log commits by itself: a save is visible once its commit segment is.
Because the log replays over pages until `meta` moves past it, checkpoints can
overwrite `page/<n>` in place. The map, head, intent, IDs and retirement
accounting are gone. What remains is the log codec, replay rules, a bounded
checkpoint slice per save and confirmation on weak backends. See
[PROTOCOL.md](PROTOCOL.md).

"Fix" and "fold" from the v11 delta-log review are part of the design:

- **Fold** is a checkpoint slice of about 12 pages, issued in the same round trip
  as the commit. An owner with a `PageCache` records it in `meta` in the next
  save's round trip; without a cache a checkpointing save takes two round trips.
  The worst save went from 11–17 round trips (v11 fold) to 2–3.
- **Fix:** with a cache, open probes the log together with `meta`, and skips
  confirming the tail and `meta` this owner wrote. Backends that report
  `durable_when_visible()` never confirm. Steady-state opens are one round trip.

## Changes

- New format v12: `meta` (`SM12`), log segments (`SL12`) with multi-segment
  transactions and a commit flag, pages at `page/<n>`.
- `src/log.rs`: segment and checkpoint-record codecs, probing and replay that
  stops at debris from failed saves.
- `src/snapshot.rs`: reads (log image, else cache, else page key) and the save
  pipeline. The commit and checkpoint groups run concurrently. Large saves
  (over 256 KiB) checkpoint themselves after committing. Garbage is only deleted
  below a durable `meta`.
- `src/cache.rs`: `PageCache` replaces `ObjectCache`. Pages are mutable now, so
  the cache keeps the exact durable `meta` bytes as a validity token. It resets
  on any foreign checkpoint and remembers acknowledged commits and claims.
- `KvStore::durable_when_visible()` (default false) lets strong backends skip
  confirmation.
- Removed `map_codec.rs`, `pending.rs`, the radix map, object IDs, the intent and
  `MAX_PENDING_BYTES`.
- Measurement fix: concurrent phases issue their requests when queued, and the
  engine read batch uses `join_all`. Before this, the simulator timed some
  requests from their first poll, after `FuturesUnordered` had already yielded,
  which overstated parallel phases. Re-measuring v11 with the fix changed none of
  its numbers.
- Benchmarks and analyzers use page, log and meta classes. Baselines are
  checkpointed bulk loads. `log_shipping` compares v12 by policy, backend and cache
  with the recorded v11 design.

## Validation by mutation

The crash campaign was run against four deliberately broken variants. Each was
caught:

| Broken rule | Result |
|---|---|
| Open skips confirming the log tail | Campaign fails |
| `meta` passes a checkpoint before its pages are acknowledged | Campaign fails |
| Garbage below a not-yet-durable `meta` is deleted | Campaign fails |
| A large save writes its pages in place before its commit | Campaign fails |

## Executed validation

Rust/Cargo 1.97.0, locked dependencies, Linux x86_64.

| Check | Outcome |
|---|---|
| `cargo test --locked` | 9 tests and one compile-checked rustdoc example pass |
| `cargo test --locked --features simulation` | 48 tests pass: 28 unit, 19 integration, one public API; one rustdoc example also passes |
| Interruption campaigns | 1,548 cases (below) |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Pass |
| `cargo fmt --check` | Pass |
| `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features --document-private-items` | Pass |
| Amplification benchmark, three repeats | 3,420 observations / 684 phases / 104 configurations; counters reconcile |
| Page-compression benchmark, three repeats | 504 SQL observations / 84 Raw-LZ4 pairs / 72 codec timing samples; traffic agrees |
| Small-database benchmark, three repeats | 495 observations; SQL/reopen checks pass |
| Log-shipping benchmark, two repeats | 16 v12 configurations × 128 sessions plus re-measured v11; repeats identical; final reads verify every write |
| All four benchmark analyzers | Pass on v12 evidence |

Interruption cases:

- 984 storage-level saves: every put and delete of appends, checkpoints, garbage
  collection, `meta` writes and large self-checkpointing saves, in three outcomes,
  with and without a crash and an owner cache. Each case checks atomicity, a
  follow-up save that survives a crash, and no segment below `deleted_below`.
- 102 SQL-level saves with `integrity_check`.
- 396 read failpoints across six workloads.
- 28 open-read failpoints and 4 interrupted confirmations.
- 6 commit and checkpoint errors.
- 28 private-checkpoint interruptions.

## Remaining limits

Exclusive ownership and settlement of outstanding requests remain host
requirements. A `PageCache` must be dedicated to one namespace. Log replay and
dirty pages have no memory budget. Memory budgets, request fencing,
real-backend latency and process crash/power-loss validation remain future work.

See [format](FORMAT.md), [protocol](PROTOCOL.md), and
[benchmark methodology and evidence](../benches/README.md).

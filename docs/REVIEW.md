# Review and validation — 2026-10-11

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
[DESIGN.md](DESIGN.md).

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
  the cache keeps the exact durable `meta` bytes as a validity token and
  remembers acknowledged commits and claims. After a foreign checkpoint, or this
  owner's failed save, open reads the segments between the cache's base and the
  new log start alongside replay and refreshes the cached pages they changed. It
  drops every page only when those segments are collected or exceed one probe
  batch.
- `PageCache` is process-wide: pages in memory and on a local foyer disk tier,
  shared by every namespace under one budget. `PageCache::namespace(key)` returns
  a `NamespaceCache` with that namespace's state. Page keys are (generation,
  page, version) and never reused, so correctness does not depend on the disk
  tier's deletes. Idle engines are capped across namespaces.
- `KvStore::durable_when_visible()` (default false) lets strong backends skip
  confirmation.
- Reads: a cold open prefetches every page of a database of at most
  `StorageOptions::prefetch_pages` (64) alongside replay; three consecutive page
  reads from the backend start readahead in a window that doubles up to 64; an
  owner probes the log exactly to where it left it. Up to 64 mutations of a
  save group are in flight, so saves with a cache take one round trip.
- Working-set hint: checkpointing saves write the pages their owner read most
  usefully (page 0, interior B-tree pages, recent leaves; 32 by default, at most
  256) to `v12/hint`, and a cold open of a database larger than 16 pages fetches
  them alongside replay. On 10,000 rows a cold point-read session went from 5.7
  to 3.2 round trips. The hint is advisory and outside the invariants. The
  compression benchmark disables it, because checkpoint timing (and so the hint)
  depends on encoded sizes.

## Independent review

Three independent reviews (storage protocol, engine integration, efficiency)
read the code and tried to break it, each confirming findings with temporary
tests. The storage protocol review found no bug: about 2,700 fuzzed runs with
faults on every request (around 550,000 injected), crashes and three
alternating owners, at the snapshot and SQL levels. The engine review found
three bugs, all fixed with regression tests:

- A read-only session that overlapped a save parked its older engine under the
  newer commit; the next session read stale data and its save erased the other
  commit. Engines now park only at their own position, and only when it is the
  owner's latest acknowledged commit.
- With a page size above 4096, a saved database could not be reopened: the
  engine's synchronous header read spanned an unloaded block. Open now keeps
  every block of the first page resident.
- Per-connection state (PRAGMA settings, the temp schema) carried over to the
  next session through a reused engine. Sessions that change it no longer park.

Minor fixes: a new namespace state no longer probes and refreshes from log
position 1, and cache identities start at a random value.
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

The crash and handover campaigns were run against deliberately broken variants. Each was
caught:

| Broken rule | Result |
|---|---|
| Open skips confirming the log tail | Campaign fails |
| `meta` passes a checkpoint before its pages are acknowledged | Campaign fails |
| Garbage below a not-yet-durable `meta` is deleted | Campaign fails |
| A large save writes its pages in place before its commit | Campaign fails |
| Open keeps cached pages without refreshing them after a foreign checkpoint | Handover campaign fails |
| The refresh loses the first changed segment's images | Handover campaign fails |
| The refresh trusts whichever changed segments are still present | Handover campaign fails |
| An engine parks at the cache's latest commit instead of its own position | Overlapping-session test fails |
| Only the first block of the first page is resident | Large page size test fails |
| Sessions that change connection state still park | Connection state test fails |
| Page keys carry no generation | Six cache, crash and handover tests fail |
| No prefetch, no readahead, no exact probe, or 16 mutations in flight | The matching round-trip test fails |
| The hint is never written, or not used at open | Working-set hint test fails |
| A continued session keeps a stale resident header page | Every commit-durability test fails |
| A commit leaves values fetched ahead for the pages it rewrote | Continued-snapshot test fails |
| Dropping checkpointed transactions also drops pages a later one changed | Continued-snapshot test and three commit-durability tests fail |
| After a failed checkpoint, a continued session drops its transactions anyway | Commit campaign fails |

## Executed validation

Rust/Cargo 1.97.0, locked dependencies, Linux x86_64.

| Check | Outcome |
|---|---|
| `cargo test --locked` | 9 tests and one compile-checked rustdoc example pass (cache tests need the `simulation` feature) |
| `cargo test --locked --features simulation` | 71 tests pass: 37 unit, 32 integration, two public API; one rustdoc example also passes |
| Interruption campaigns | 2,169 cases (below) |
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
- 621 commit-per-transaction cases (`Durability::Commit`): every put and delete
  of a 30-transaction session failed in three outcomes, with and without an
  owner cache. Every acknowledged transaction survives a crash, the failed one
  may, nothing else appears, and the namespace stays writable.
- Owner handover: two owners, each with its own cache, alternate over 400
  random tenures with failed saves, lost unconfirmed values and restarts, and
  every open checks every page. It also passed 200 seeds (80,000 tenures).
- 28 private-checkpoint interruptions.

## Remaining limits

Exclusive ownership and settlement of outstanding requests remain host
requirements. A namespace must use one stable key with `PageCache::namespace`. Log replay and
dirty pages have no memory budget. Memory budgets, request fencing,
real-backend latency and process crash/power-loss validation remain future work.

See [format](FORMAT.md), [design](DESIGN.md), and
[benchmark methodology and evidence](../benches/README.md).

# Design

SQLite-compatible SQL (Turso's `turso_core` engine, unmodified) over an
asynchronous key/value store, for namespaces with one owner at a time, such as
a Restate virtual object.

## Constraints

- **The backend is plain KV.** Whole-value `get`, and atomic, durable
  single-key `put` and `delete`. No multi-key transactions, compare-and-swap,
  listing or sync.
- **Outcomes can be uncertain.** A failed or cancelled mutation may have no
  effect, a visible but not durable effect, or a durable effect. Values are
  never torn.
- **One owner at a time.** The host guarantees exclusive ownership of a
  namespace and settles every outstanding request before ownership moves. This
  crate does not fence.
- **Round trips dominate.** A save or open should cost one round trip in the
  steady state.

## The model

A namespace holds three kinds of keys:

| Key | Holds |
|---|---|
| `v12/meta` | Where the log starts (`log_start`), which segments are already deleted (`deleted_below`), and the database size at `log_start`. Absent means start 1, empty. |
| `v12/log/<seq>` | Log segments. A save is one transaction of page images, in consecutive segments; only the last has the commit flag. |
| `v12/page/<n>` | Page *n* as of some checkpoint, overwritten in place. Absent means zeros. |

**Read rule.** Page *n* is its latest image in the committed log from
`meta.log_start`, else `page/<n>`, else zeros.

Everything else follows from five invariants:

1. **Commit.** A transaction is committed once its commit segment is durable.
   That segment is written only after the transaction's other segments are
   acknowledged.
2. **Build on durable.** A transaction is appended only after its predecessor's
   commit is durable: acknowledged to this owner, or confirmed by open.
3. **Checkpoint before recording.** `meta.log_start` moves past a transaction
   only after every page it changed is acknowledged at its page key, with that
   image or a newer committed one.
4. **Delete behind a durable `meta`.** A segment is deleted only after a durable
   `meta` has moved past it.
5. **Overwrite only what the log covers.** A checkpoint writes only images of
   committed transactions that are still in the replayed log.

So a page key may hold a newer image than `meta` says, which is harmless: the
log still holds that transaction and replays over it (5). It can never hold an
image the log does not cover once `meta` has moved (3). Replay never starts at a
deleted segment (4).

Exact encodings are in [FORMAT.md](FORMAT.md).

## Save

A session writes SQL into a private view (dirty pages and a private engine WAL).
`save()` checkpoints that private WAL inside the engine, takes the changed
pages, and commits them:

1. **Commit.** Encode the changed pages as one transaction at the log's next
   sequence. Put all segments but the last concurrently, then the last. Up to
   1 MiB of images is one segment, so usually one put.
2. **Checkpoint slice, in the same round trip.** When the log holds more than
   `checkpoint_after` transactions (default 4) or more than 256 KiB, take whole
   transactions from the oldest until about 12 pages are covered. Put each
   page's latest committed image to its page key; holes become deletes.
3. **Garbage, in the same round trip.** Delete segments in
   `[deleted_below, log_start)` of the durable `meta`.
4. **Record.** Once the slice is acknowledged, `meta` may move past it.
   - Without a cache, write `meta` in a second round trip.
   - With a cache, remember the acknowledged position as a *claim*, and write
     `meta` in the next save's round trip, alongside its commit.
5. **Large saves.** A save over 256 KiB checkpoints the whole log, including
   itself, right after committing, then writes `meta`. Later opens do not replay
   a bulk load.

At most 16 mutations of distinct keys are in flight per group. `Ok(SaveReport)`
means committed. `cleanup_error` means committed but checkpoint work failed; a
later save retries it. Any other error leaves the commit outcome unknown.

## Open

1. Read `meta`. With a cache, also read the first batch of log keys at the
   expected start in the same round trip.
2. Replay from `log_start` (or the cache's claim) in concurrent batches of
   `checkpoint_after + 2` keys (at most 16). Apply a transaction at its commit
   segment. Stop at the first absent key, or at a segment that does not continue
   the current transaction: that is debris from a failed save, which the next
   commit overwrites.
3. **Confirm** (weak backends only). Re-put the last commit segment and the
   `meta` that was read, so that nothing exposed can later vanish (invariant 2).
   This is skipped when the backend reports `durable_when_visible()`, or when the
   cache shows this owner wrote and had them acknowledged.

## Failures

Every step is idempotent, and a crash anywhere leaves state that replay reads
correctly.

| Interrupted step | State afterwards | Recovery |
|---|---|---|
| Non-commit segment | Debris; transaction uncommitted | Ignored by replay; overwritten by the next commit |
| Commit segment | Absent, or present and therefore committed | Replay decides; open confirms a present tail |
| Checkpoint page put | Old or new committed image | The log still replays over it |
| `meta` put | Old or new record; both valid | Open confirms what it read |
| Garbage delete | Segment present or absent below a durable start | Deleted again later |

## Engine integration

The engine sees two virtual files, the database and its WAL, served by `KvIo`:

- **Reads** of the database file go to the committed view (log image, cache or
  page key). Adjacent reads are batched (up to 16 concurrently) without crossing
  write, truncate or sync barriers.
- **Writes, truncation and sync** stay in private memory. File sync is only an
  ordering barrier. Nothing reaches KV before `save()`, and the engine WAL never
  does.
- **Execution.** The caller's task drives the engine's state machines and
  awaits KV I/O. Queries yield every 64 engine steps; one engine call cannot be
  preempted. The engine also reads synchronously for metadata. Open preloads the
  header page for those reads, and any other synchronous read that would need KV
  fails and poisons the session.
- **Sessions.** Query errors and cancellation poison a session. `save()`
  consumes it, and dropping it discards unsaved work.

## Owner cache

One `PageCache` per process. `PageCache::namespace(key)` returns a
`NamespaceCache` for one namespace, which a session gets through
`StorageOptions::cache`.

**Pages.** Checkpointed pages are kept in memory and spill to a local disk
file ([foyer](https://github.com/foyer-rs/foyer)), one budget for all
namespaces. Values are stored encoded (LZ4) by default, or decoded with
`PageCacheConfig::decoded`.

**Per-namespace state:**
- the exact `meta` bytes this owner last read or wrote;
- the *base*: the log start of that `meta`;
- a claim (an acknowledged checkpoint not yet in `meta`);
- the last acknowledged commit;
- a parked engine.

The last commit and the `meta` bytes skip confirmation, and the claim lets
`meta` ride with the next commit. Together these make the steady state one
round trip per open and per save.

**Correctness rule.** A cached page is current unless a committed transaction
at or after the base changed it. While that transaction is in the replayed log,
the log overrides the cache, so only a checkpoint that moves `meta` past it can
expose a stale value. This owner's checkpoints write through the cache, so only
another writer (or this owner's failed save) can do that.

**After another writer.** If open reads a `meta` other than the one remembered,
it reads the segments from the base to the new log start, alongside replay, and
replaces the cached pages they changed. Segments below a durable log start are
committed and never rewritten, only deleted, so a present segment is trustworthy.
If any is gone, or there are more than 16, the namespace drops all its pages.
Garbage collection deletes them one save after `meta` moves, so a brief other
owner (around 7 saves with default settings) leaves the cache warm.

**Keys are never reused.** The disk tier writes in the background, and a delete
does not stop a write already queued. So a cache key is (generation, page,
version): changing or invalidating a page bumps its version, and dropping all of
a namespace's pages starts a new generation. Stale entries are unreachable and
age out. The disk tier starts empty in every process, because the per-namespace
state that validates it is not persisted.

**Engine reuse.** A session that ends cleanly (a successful save, or read-only)
parks its engine. The next open reuses it if `meta` matches, the log ends where
that engine left it, and its I/O is clean. This keeps the parsed schema and the
engine's page cache warm, which cuts CPU per session about 5×. Parked engines
and namespace states are capped per process, least recently used first;
dropping either only makes a namespace cold.

**Threads.** foyer's disk I/O runs on the host's tokio runtime, or on a
one-thread runtime of its own. It is the only background work.

## Costs

| Operation | Round trips | Mutations |
|---|---|---|
| Save, with cache | 1 | 1 commit put; checkpoint pages, deletes and `meta` when due |
| Save, without cache | 1, or 2 when it checkpoints | Same, with `meta` in the second round trip |
| Large save (over 256 KiB) | 2 or more | Segments, then every logged page and `meta` |
| Open, with cache | 1 | None |
| Open, fresh owner, weak backend | 2 to 3 | Re-put of the last commit and `meta` |
| Cold page read | 1 get | None |

Each page is written twice, once to the log and once at checkpoint, and opens
replay at most a short log. Measurements: [log shipping](../benches/LOG_RESULTS.md),
[cache](../benches/CACHE_RESULTS.md).

## Limits and open questions

- **Backend choice.** If the backend commits several writes atomically (for
  example, a store whose writes within one invocation commit together), a save
  is atomic without the log, and most of this protocol is unnecessary. The
  design assumes it is not.
- **Fencing.** Exclusive ownership is assumed. A delayed write from a former
  owner could overwrite newer data. A conditional put or an ownership epoch on
  commit and `meta` writes would close that gap.
- **Memory.** Dirty pages, the private WAL, the replayed log and query results
  have no byte budgets.
- **Integrity.** Encodings are validated structurally; there are no checksums.
- **Format.** Only v12, with no migration.
- **Validation.** Crash, interruption and handover campaigns run against an
  in-memory simulator; there are no process-kill or power-loss tests. See
  [REVIEW.md](REVIEW.md).

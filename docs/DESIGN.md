# Design

SQLite-compatible SQL (Turso's `turso_core` engine, unmodified) over an
asynchronous key/value store, for namespaces with one owner at a time, such as
a Restate virtual object.

It serves two kinds of host:

- **A journaled handler**, which a failure replays from the start. The
  database is part of the invocation's state: a session is durable as a whole at
  its end (`Durability::Save`), and a failed attempt leaves nothing behind.
- **An object without a journal**: a key, its own database, and calls to other
  objects or services, with at-least-once delivery. The database is the
  object's only durable record, so it behaves like SQLite: each SQL
  transaction is durable when it commits (`Durability::Commit`), and
  `Session::transaction` scopes one to a closure.

Both write the same format through the same protocol; only how much one save
covers differs.

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
| `v12/hint` | Advisory: pages a cold open should fetch. Never needed for correctness. |

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
A save checkpoints that private WAL inside the engine, takes the changed pages,
and commits them:

1. **Commit.** Encode the changed pages as one transaction at the log's next
   sequence. Put all segments but the last concurrently, then the last. Up to
   1 MiB of images is one segment, so usually one put.
2. **Checkpoint slice, in the same round trip.** When the log holds more than
   `checkpoint_after` transactions (default 4) or more than 256 KiB, take whole
   transactions from the oldest until about 12 distinct pages are covered. Put each
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

At most 64 mutations of distinct keys are in flight per group, so a commit
and its checkpoint slice, garbage and `meta` take one round trip. `Ok(SaveReport)`
means committed. `cleanup_error` means committed but checkpoint work failed; a
later save retries it. Any other error leaves the commit outcome unknown.

## When a save happens

`StorageOptions::durability` decides what one save covers:

- **`Durability::Save`** (default): the whole session. SQL `COMMIT` stays
  private until `Session::save()`, which commits everything as one log
  transaction and ends the session. Dropping the session discards it all. This
  fits a host that already makes the unit of work atomic, such as a journaled
  handler that a failure replays from the start.
- **`Durability::Commit`**: every SQL transaction, as in SQLite. When a
  statement leaves the engine in autocommit mode with private changes (an
  autocommit write, or `COMMIT`), the session saves before the statement
  returns and continues on the committed view. Reads never save. Dropping the
  session loses only an open transaction. `Session::transaction(closure)` wraps
  `BEGIN`/`COMMIT`, and rolls back when the closure returns `Err`.

A continuing session moves its view forward locally instead of reopening: the
new transaction joins its replayed log, and transactions a checkpoint covered
leave it once the recorded or claimed log start passes them. After a cleanup
error it keeps them, so reads still come from the log. Each SQL commit costs
one round trip with a cache (checkpoint work rides along). Commits are
sequential: invariant 2 forbids starting the next before the previous one is
acknowledged.

**Retries without a journal.** With at-least-once delivery, a handler that
fails after a commit runs again on top of it. Two patterns keep that correct:

- **Idempotent requests.** In the same transaction as the request's effects,
  record its id, and stop if it was already there:
  a plain `INSERT` into a table keyed by the id fails on a duplicate, which
  rolls back `Session::transaction`, or `ON CONFLICT DO NOTHING RETURNING id`
  returns no row for one. The dedupe record and the effects commit atomically,
  because they are one log transaction.
- **Outbox for calls.** Commit the intent to call another object or service
  as a row, then send it, and delete the row once the call is acknowledged. A
  retry finds the row and resends, so the callee must deduplicate too.

A commit whose statement returned an error has an unknown outcome: reopen and
look (the dedupe row says whether it landed) rather than retrying blindly.

**Choosing.** Per-save durability costs one round trip per session and
keeps a failed attempt invisible; per-commit durability costs one round trip
per write transaction and makes progress durable as it happens. With a
journal, prefer per-save: a replay re-runs the SQL against the state it
started from. Without one, per-commit is the only choice that does not lose
acknowledged work when a later step fails.

## Open

1. Read `meta`. With a cache, also read the log keys at the expected start in
   the same round trip: exactly to where this owner left the log, plus one.
2. Replay from `log_start` (or the cache's claim) in concurrent batches of
   `checkpoint_after + 2` keys (at most 64). Apply a transaction at its commit
   segment. Stop at the first absent key, or at a segment that does not continue
   the current transaction: that is debris from a failed save, which the next
   commit overwrites.
3. **Confirm** (weak backends only). Re-put the last commit segment and the
   `meta` that was read, so that nothing exposed can later vanish (invariant 2).
   This is skipped when the backend reports `durable_when_visible()`, or when the
   cache shows this owner wrote and had them acknowledged.

**Cold opens prefetch.** The engine reads one dependent page at a time, so
without warm cache pages each B-tree level of a query would cost a round trip.
An owner that knows nothing about the namespace therefore reads `v12/hint`
together with `meta`, and fetches pages alongside replay:

- a database of at most `prefetch_pages` pages (default 16) whole;
- otherwise, the pages the working-set hint lists.

Pages the log holds are dropped from the prefetch; the log wins as always.

**The working-set hint.** An owner (or, without a cache, a session) records the
pages its sessions read. A save that checkpoints writes the most useful of
them, if the list changed, in the same round trip: page 0, then interior B-tree
pages, then the most recently read leaves, at most `hint_pages` (default 32,
at most 256), as ascending LEB128 deltas (about 1–4 bytes per page). The hint
is advisory and outside the invariants: it is never confirmed or ordered
against other writes, and a stale, missing or malformed hint only wastes or
saves gets. It stays out of `meta`, which is on the correctness path.

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

- **Reads** of the database file go to the committed view: log image, then
  cache, then pages fetched ahead, then the page key. The engine issues one
  dependent read at a time; adjacent queued reads are batched (up to 16) without
  crossing write, truncate or sync barriers. After three consecutive page reads
  from the backend, each further get also fetches the following pages, in a
  window that doubles up to 64, so a scan costs a few round trips rather than
  one per page.
- **Writes, truncation and sync** stay in private memory. File sync is only an
  ordering barrier. Nothing reaches KV before a save, and the engine WAL never
  does.
- **Execution.** The caller's task drives the engine's state machines and
  awaits KV I/O. Queries yield every 64 engine steps; one engine call cannot be
  preempted. The engine also reads synchronously for metadata. Open preloads the
  first page (every 4096-byte block of it, for larger page sizes) for those
  reads, and any other synchronous read that would need KV fails and ends the
  session.
- **Statements.** A string of several statements runs them in order, each
  under the same rules as on its own. `query_with`/`execute_with` bind
  parameters to one statement, so values never become SQL text.
- **Errors.** An SQL error (syntax, a constraint, a missing table, wrong
  parameters) undoes its statement, as in SQLite: the session stays usable and
  an open transaction continues. The engine undoes a failed statement itself,
  so this holds when no I/O failed and none is pending; the adapter records
  every failed request and synchronous read to tell. Any other error (storage,
  a limit, a cancelled call, an engine anomaly) ends the session, and an error
  from a statement that saves leaves its commit outcome unknown.
  `Session::is_usable` says which happened.
- **Sessions.** `save()` consumes a session, and dropping it discards unsaved
  work. Inside
  `Session::transaction`, a statement that ends the transaction (`COMMIT`,
  `END`, `ROLLBACK`) fails and ends the session before anything is durable,
  and so does dropping or unwinding the transaction future.

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
If any is gone, or there are more than 64, the namespace drops all its pages.
Garbage collection deletes them one save after `meta` moves, so a brief other
owner (around 7 saves with default settings) leaves the cache warm.

**Keys are never reused.** The disk tier writes in the background, and a delete
does not stop a write already queued. So a cache key is (generation, page,
version): changing or invalidating a page bumps its version, and dropping all of
a namespace's pages starts a new generation. Stale entries are unreachable and
age out. A value read from the backend is cached under the key taken before the
read, and only if that key is still current, so a read that raced a
replacement cannot store the old value under the new version. The disk tier
starts empty in every process, because the per-namespace state that validates
it is not persisted.

**Sessions of one owner.** Sessions may overlap, such as a writer and readers
for a virtual object's shared handlers, if they share the namespace's handle;
one namespace keeps one state while any handle holds it.
- *One writer.* A commit is refused when another session committed since this
  one opened, rather than writing the log position that session used.
- *Isolation.* A session reads the state it opened at: pages outside its log
  come from page keys and the cache, which checkpoints overwrite. So each open
  session registers its log position, and a checkpoint writes no image of a
  transaction after the oldest other session's position (like SQLite's WAL read
  marks). A parked engine releases its position. Checkpoints resume when the
  reader ends.
- Without a cache there is no shared state, so sessions must not overlap.

**Engine reuse.** A session that ends cleanly (a successful save, or read-only)
parks its engine, but only if that engine holds the owner's latest
acknowledged commit (an overlapping session's engine may be older) and its SQL
changed no per-connection state (`PRAGMA`, `ATTACH`, `DETACH`, the temp schema).
Only the database is kept: the next session connects afresh, so connection
counters (`last_insert_rowid()`, `changes()`) and settings never carry over.
The next open reuses it if `meta` matches, the log ends where that engine left
it, and its I/O is clean. This keeps the parsed schema warm, which cuts CPU per
session several-fold ([cache results](../benches/CACHE_RESULTS.md)). A parked
engine releases its view of the log (the next open replaces it), so what it
holds is its parsed schema: measured at 40–110 KiB from one to 50 tables. Parked
engines share a byte budget (`PageCacheConfig::engine_bytes`, 64 MiB), each
weighed as 48 KiB plus 16 bytes per byte of schema SQL, measured when an engine
opens and after a commit that may have changed the schema. Namespace states are
capped by count. Least recently used go first; dropping either only makes a
namespace cold.

**Stores that acknowledge before storing.** All of the above trusts what the
backend acknowledged. A store may instead acknowledge writes as it orders them
and lose an unconfirmed tail when a request fails, such as Restate state
writes, which an attempt that dies loses from its first unstored command on.
The process, and this cache, then outlive writes that do not exist: a claim
whose pages were lost would let the next save move `meta` past segments that
no page key covers, and delete them. Such a host confirms the writes before the
cache is used again, or calls `NamespaceCache::reset`, which drops the
namespace's `meta`, claim, commit, pages and parked engine
([`tests/integration/ordered.rs`](../tests/integration/ordered.rs)).

**Threads.** foyer's disk I/O runs on the host's tokio runtime, or on a
one-thread runtime of its own. It is the only background work.

## Costs

| Operation | Round trips | Mutations |
|---|---|---|
| Save, with cache | 1 | 1 commit put; checkpoint pages, deletes and `meta` when due |
| Save, without cache | 1, or 2 when it checkpoints | Same, with `meta` in the second round trip |
| SQL commit with `Durability::Commit` | Same as a save | Same as a save |
| Large save (over 256 KiB) | 2 or more | Segments, then every logged page and `meta` |
| Open, with cache | 1 | None |
| Open, fresh owner, weak backend | 2 to 3 | Re-put of the last commit and `meta` |
| Cold open | Same as open; hinted pages (or a small database whole) arrive with replay | None, plus the hint put in checkpointing saves |
| Cold page read | 1 get; sequential reads fetch up to 64 ahead | None |

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
- **Memory.** A session's private changes (`max_private_bytes`, 256 MiB) and a
  statement's result rows (`max_result_bytes`, 64 MiB) are capped; exceeding
  either fails the statement. Past 4 MiB of log, checkpoint slices grow to 256
  pages, so a log grown by failed checkpoints shrinks again and replay stays
  bounded. Parked engines share a byte budget, by a measured estimate.
- **Integrity.** Storage is assumed reliable: a value reads back as written.
  Decoding checks structure and bounds sequence numbers, sizes and page
  numbers, so a bug cannot overflow arithmetic or size a huge view; there are
  no checksums.
- **Format.** Only v12, with no migration.
- **Validation.** The protocol is specified in TLA+ and model-checked within
  small bounds, and a monitor checks every request of the simulator's crash,
  interruption and handover campaigns against it
  ([spec/README.md](../spec/README.md)). There are no process-kill or
  power-loss tests, and no real backend yet. See [REVIEW.md](REVIEW.md).

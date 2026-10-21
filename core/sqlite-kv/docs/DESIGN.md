# Design

SQLite-compatible SQL (Turso's `turso_core` engine, unmodified) over a
Restate virtual object's state: a key/value namespace with one owner at a
time.

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

- **The backend is a Restate object's state** ([`kv`](../src/kv.rs)):
  whole-value `get`, and `put` and `delete` of one key, atomic, queued in
  issue order; `sync` returns once every write before it is stored. No
  multi-key transactions, compare-and-swap or listing.
- **A crash keeps a prefix.** Of the writes since the last sync, a crash (or
  a failed sync) keeps the first few and loses the rest: never a write
  without the ones before it. Values are never torn. Reads see every write
  this owner issued.
- **One owner at a time.** Restate runs one exclusive handler per object key,
  and fences a former attempt's writes. This crate does not fence.
- **Round trips dominate.** A commit or open should cost one round trip in
  the steady state.

## The model

A namespace holds four kinds of keys:

| Key                | Holds                                                                                                                                                                                                                |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `v15/commit`       | The commit record: the version, the epoch, the directory of files (records and paged files, with their sizes), the log since the last checkpoint (its tail inline, the rest naming stored values).                    |
| `v15/log/<n>`      | A stored value: segments the record's inline log sealed, or a large transaction's. Immutable: written once, before the record that names it, deleted once no record names it.                                        |
| `v15/page/<f>/<n>` | Page _n_ of paged file _f_ as of some checkpoint, overwritten in place. Absent means zeros.                                                                                                                          |
| `v15/hint`         | Advisory: pages a cold open should fetch. Never needed for correctness.                                                                                                                                              |

**Read rule.** Page _n_ of a paged file is its latest image in the record's
log, else `page/<f>/<n>`, else zeros. The SQL database is the paged file
`db`; the host's files (`Journal::file`) are others, written in the same
transactions. Everything below says "page" for a page of any file: a page is
a file and a number. A file with a base (a VM's memory over its golden image)
reads its base's page where it has none; see `FORMAT.md`.

**The record is the commit point.** A transaction is committed once a record
that holds it is durable. Each put of the record is one transaction more
than the last (the next version) or a checkpoint (the next epoch, with an
empty log). The store keeps a prefix of the writes, so a write may rely on
every write issued before it. Everything else follows from four rules of
issue order:

1. **Values before the record that names them.** A stored value is put
   before the first record that names it.
2. **Build on the record.** A record holds the log of the record before it
   (the one this owner put last, or the one open read), plus one
   transaction.
3. **Pages before the checkpoint record, after the commit.** A checkpoint
   writes each page the log changed, with the log's latest image of it,
   after the record whose log holds them and before the record that empties
   the log. So whatever prefix of its page writes a crash keeps, the record
   kept with them has a log that replays over them.
4. **Fresh values, deleted after their last record.** A stored value is put
   only at a number no record has allocated, and deleted only after a record
   that no longer names it; that record lists it as garbage, so a crash
   between the two leaks nothing.

So no write waits for another, and a crash at any point leaves a record whose
log replays over whatever pages and values it kept.

Exact encodings are in [FORMAT.md](FORMAT.md).

## Save

A session writes SQL into a private view (dirty pages and a private engine WAL).
A save checkpoints that private WAL inside the engine, takes the changed pages,
and commits them:

1. **Encode.** The changed pages as one transaction of segments (up to 240
   pages each), compressed against the log (below).
2. **Garbage.** Delete the stored values the record lists as garbage: the
   record before named them, this one no longer lists them.
3. **Inline or stored.** A transaction whose segments total at most
   `inline_log_bytes` (1 KiB) goes inline in the record. A larger one goes to
   fresh `log/<n>` keys, a segment each, which the record names.
4. **Seal.** Once the record's inline parts would pass that budget, the
   commit puts them into one stored value, and the record names it in their
   place. So every commit rewrites a record of a few KiB, while the log grows
   long between checkpoints.
5. **Commit.** Put the record: the previous record's log plus this
   transaction, at the next version, with the records the host set (a large
   one stored first, as a value of its own), listing as garbage the values it
   names no more.
6. **Checkpoint the whole log, now and then.** When the log then holds more
   than `checkpoint_after` transactions (32) or `checkpoint_log_bytes`
   (64 KiB), each page's latest image to its page key (holes become deletes),
   the hint, and a record with an empty log at the next epoch, whose garbage
   takes the stored values the log named; the next commit deletes them. This
   owner's cache keeps its pages into the next epoch: only the log's pages
   changed, and it writes those through.
7. **Sync.** One sync stores all of it: one round trip, whatever the save
   wrote.

**Compression.** A commit rewrites the same few pages as the ones before it,
each a little changed. A segment's images are one zstd frame whose
dictionary is the previous image of each of its pages in the log: so a page
costs about what changed in it. Replay rebuilds those images as it goes, from
the last checkpoint, where the previous images start empty.

**Bytes.** The record is rewritten whole by each commit, so its inline log is
the cost of a commit: the seal bounds it, and a transaction's segments are
written about twice (inline, then sealed), its pages once more at a
checkpoint. Pages are zstd frames by default (`PageCompression::Zstd`).
`Ok(SaveReport)` means committed; an error leaves the commit outcome unknown.

**Effects.** A host may commit bytes of its own with a transaction
(`Session::commit_with`): they ride in the same record put, so they are
durable exactly when the transaction is, and stay, through checkpoints, until
a later commit replaces them. The Restate SDKs keep a transaction's sends
there, as its outbox, and make them once it commits. These are the record
`effects`; a host may set any record (`Journal::record`) and write paged
files of its own (`Journal::file`), committed by the next `Session::seal`. A
commit with no page changes adds no log transaction, so a host's turns do
not checkpoint SQL.

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
new transaction joins its replayed log, and a checkpoint empties it. Each SQL
commit costs one round trip. Commits are sequential: each builds on the
record of the one before (rule 2), once its sync returned.

**Retries without a journal.** With at-least-once delivery, a handler that
fails after a commit runs again on top of it. Two patterns keep that correct:

- **Idempotent requests.** In the same transaction as the request's effects,
  record its id, and stop if it was already there:
  a plain `INSERT` into a table keyed by the id fails on a duplicate, which
  rolls back `Session::transaction`, or `ON CONFLICT DO NOTHING RETURNING id`
  returns no row for one. The dedupe record and the effects commit atomically,
  because they are one log transaction.
- **Outbox for calls.** Commit the intent to call another object or service
  with the transaction, as effects, then make the call, and drop it from a
  later commit once the call is stored. A retry finds it and calls again, so
  the callee must deduplicate too.

A commit whose statement returned an error has an unknown outcome: reopen and
look (the dedupe row says whether it landed) rather than retrying blindly.

**Choosing.** Per-save durability costs one round trip per session and
keeps a failed attempt invisible; per-commit durability costs one round trip
per write transaction and makes progress durable as it happens. With a
journal, prefer per-save: a replay re-runs the SQL against the state it
started from. Without one, per-commit is the only choice that does not lose
acknowledged work when a later step fails.

## Open

1. Read the record. An owner that knows no pages of the namespace also reads
   `v15/hint`, in the same round trip.
2. Replay its log: inline segments are in hand, stored values the owner
   cache holds too, and the others are read together, in one more round trip.
3. **Cold opens prefetch.** The engine reads one dependent page at a time, so
   without warm cache pages each B-tree level of a query would cost a round
   trip. An owner whose cache holds no pages of the record's epoch fetches,
   alongside replay, a database of at most `prefetch_pages` pages (default 16) whole, or else the pages the hint lists. Pages the log holds are
   dropped from the prefetch; the log wins as always.

There is nothing to confirm or repair: the record read is durable, and
whatever a crashed save left (stored values no record names, pages a
checkpoint wrote) is either covered by the log, garbage the next commit
deletes, or numbered at or past the record's next segment, where the next
save overwrites it.

**The working-set hint.** An owner (or, without a cache, a session) records the
pages its sessions read. A save that checkpoints writes the most useful of
them, if the list changed, in the same round trip: page 0, then interior B-tree
pages, then the most recently read leaves, at most `hint_pages` (default 32,
at most 256), as ascending LEB128 deltas (about 1–4 bytes per page). The hint
is advisory and outside the invariants: it is never waited for or ordered
against other writes, and a stale, missing or malformed hint only wastes or
saves gets. It stays out of the record, which is on the correctness path.

## Readers

A reader (`Session::open_reader`) reads a namespace that another owns,
alongside it, such as a virtual object's shared handler beside the exclusive
one. It writes nothing to the store: no cache, and its SQL changes stay in
the session ([`Durability::Save`], never saved). It opens like anyone, and
reads pages lazily, so a checkpoint by the owner meanwhile could give it a
page newer than its log. `Session::is_current` tells, once it has read: the
record unchanged. A checkpoint writes only pages its record's log covers, and
moves the record after, so a reader that read an overwritten page finds the
record changed. It is conservative: any commit since open makes the reader
stale. A stale reader reads again, in a new session.

## Failures

A crash anywhere keeps a prefix of the writes since the last sync, and with
it a record whose log replays over whatever else it kept.

| Crash after                           | State afterwards                                               | Recovery                                             |
| ------------------------------------- | -------------------------------------------------------------- | ---------------------------------------------------- |
| Some garbage deletes                  | The record before, which no longer names them                  | The next save deletes the rest                       |
| Some stored values, before the record | Values at numbers no record names                              | Ignored; the next save reuses their numbers          |
| The record                            | The new record, with everything it names                       | Open reads it                                        |
| Some checkpoint pages or the hint     | Old or new images, under the new record, whose log covers them | The log replays over them; the next save checkpoints |
| The checkpoint record                 | The empty log, at the next epoch                               | Its garbage is deleted by the next save              |

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

- the exact record bytes this owner last read or wrote, and its epoch: the
  cached pages are those of that epoch;
- the version of this owner's last durable commit;
- the stored values the record names (immutable once named), so an open
  reads the record alone;
- a parked engine.

**Correctness rule.** Page keys change only under a checkpoint, and a
checkpoint writes only pages its record's log covers before it moves to the
next epoch. So a cached page of the record's epoch is current unless the log
overrides it, and the log always does when it holds the page. Open compares
the record it reads with the one it remembers: the same bytes, and this owner
continues as it left; another record of the same epoch keeps the pages (the
log covers whatever changed); another epoch drops them all. A save whose
outcome is unknown forgets the record, so the next open reads afresh.

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

- _One writer._ A commit is refused when another session committed since this
  one opened, rather than replacing the record that session wrote.
- _Isolation._ A session reads the state it opened at: pages outside its log
  come from page keys and the cache, which checkpoints overwrite. So each open
  session registers its version, and a checkpoint waits while another session
  holds an older one (like SQLite's WAL read marks). A parked engine releases
  its version. Checkpoints resume when the reader ends.
- Without a cache there is no shared state, so sessions must not overlap.

**Engine reuse.** A session that ends cleanly (a successful save, or read-only)
parks its engine, but only if that engine holds the owner's latest
durable commit (an overlapping session's engine may be older) and its SQL
changed no per-connection state (`PRAGMA`, `ATTACH`, `DETACH`, the temp schema).
Only the database is kept: the next session connects afresh, so connection
counters (`last_insert_rowid()`, `changes()`) and settings never carry over.
The next open reuses it if the record is the one it committed, and its I/O
is clean. This keeps the parsed schema warm, which cuts CPU per
session several-fold ([cache results](../benches/CACHE_RESULTS.md)). A parked
engine releases its view of the log (the next open replaces it), so what it
holds is its database's buffer pool and its parsed schema. The buffer pool is
a 3 MiB arena that turso maps outside the allocator: resident, ~4 MiB per
parked engine (a counting allocator saw only the schema's 40–110 KiB, from one
to 50 tables, and the budget had let ~1,300 engines, ~5 GiB, stay parked).
Parked engines share a byte budget (`PageCacheConfig::engine_bytes`, 64 MiB),
each weighed as 4 MiB plus 16 bytes per byte of schema SQL, measured when an
engine opens and after a commit that may have changed the schema. Namespace states are
capped by count. Least recently used go first; dropping either only makes a
namespace cold.

**Threads.** foyer's disk I/O runs on the host's tokio runtime, or on a
one-thread runtime of its own. It is the only background work.

## Costs

| Operation                            | Round trips                                                                     | Mutations                                                                                                                                                                            |
| ------------------------------------ | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Save                                 | 1, the sync                                                                     | The record (its inline tail rewritten); a seal now and then; a large transaction's segments; the garbage's deletes; when the log is long, every logged page, the hint and the record |
| SQL commit with `Durability::Commit` | Same as a save                                                                  | Same as a save                                                                                                                                                                       |
| Open, with cache                     | 1, plus 1 if the log names stored values the cache lacks                        | None                                                                                                                                                                                 |
| Cold open                            | 1, then the hinted pages (or a small database whole) and stored values together | None                                                                                                                                                                                 |
| Cold page read                       | 1 get; sequential reads fetch up to 64 ahead                                    | None                                                                                                                                                                                 |

Each page is written twice, once to the log (compressed against its previous
image, and rewritten with the record until a checkpoint) and once at
checkpoint. Measurements: [log shipping](../benches/LOG_RESULTS.md),
[cache](../benches/CACHE_RESULTS.md).

## Limits and open questions

- **The record's size.** Every commit rewrites the inline tail of the log;
  `inline_log_bytes` trades that against seals (a stored value per few KiB of
  log), and the checkpoint thresholds trade the log's length against
  checkpoints, which write each changed page. A cold open reads the whole
  log, up to `checkpoint_log_bytes`.
- **Fencing.** Exclusive ownership is assumed. A delayed write from a former
  owner could overwrite newer data. A conditional put of the record would
  close that gap.
- **Memory.** A session's private changes (`max_private_bytes`, 256 MiB) and a
  statement's result rows (`max_result_bytes`, 64 MiB) are capped; exceeding
  either fails the statement. Replay holds each
  logged page's latest image. Parked engines share a byte budget, by a
  measured estimate.
- **Integrity.** Storage is assumed reliable: a value reads back as written.
  Decoding checks structure and bounds versions, segment numbers, sizes and page
  numbers, so a bug cannot overflow arithmetic or size a huge view. A log
  segment's zstd frame has a checksum, which also catches a segment decoded
  against the wrong previous images; pages have none.
- **Format.** Only v15, with no migration.
- **Compression.** The simulator's monitor decodes every record it
  sees; the TLA+ specification does not model compression.
- **Validation.** A monitor checks every write of the simulator's crash
  and handover campaigns against the rules above, and the campaigns crash at
  every sync keeping each prefix of its writes. The TLA+ specification
  ([spec/README.md](../spec/README.md)) models this protocol over a store
  that keeps writes in order, with the owner cache, and TLC checks it in
  small bounds. The Restate crash tests kill processes;
  there are no power-loss tests. See [REVIEW.md](REVIEW.md).

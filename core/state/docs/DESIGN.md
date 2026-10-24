# Design

Transactional state over a Restate virtual object's state (a key/value
namespace with one owner at a time): records and paged files, changed through
a WAL, which a flush makes durable together, atomically. Its participants keep
changes of their own and move them into the WAL before the flush; the main one
is SQLite-compatible SQL (the [`sql`](../../sql/README.md) crate: Turso's
`turso_core` engine, unmodified) on the file `db`.

Its host is an object without Restate's journal: a key, its own database,
and calls to other objects or services, with at-least-once delivery. The
namespace is all the object keeps, and the state is how it reaches the store:
the host flushes a WAL (records, paged files and the SQL's file `db`, in one
put) when it chooses, after each SQL transaction as SQLite does, or once per
turn of everything that takes part. The sections on SQL describe the `sql`
crate, which keeps its engine's state private until it flushes into a WAL.

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
- **Round trips dominate.** A flush or open should cost one round trip in
  the steady state.

## The model

A namespace holds two kinds of keys:

| Key            | Holds                                                                                                                                        |
| -------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `v16/root`     | The root, the commit point: the version, the directory of records and paged files, the small records, and the top of every file's chunk map. |
| `v16/blob/<n>` | One zstd frame, written once and never changed: a chunk's base or delta, a chunk map's node, or a large record.                              |

**Chunks.** A paged file is a row of chunks of `chunk_pages` pages (4,
16 KiB), as many as its size needs, at most `MAX_FILE_BYTES` (10 GiB). A
chunk is stored as a _base_, its pages compressed against its file's base
pages if it has one (a VM's memory over its golden image) or against nothing,
and maybe a _delta_: its pages now, compressed with the base as zstd's
reference prefix, so a delta costs about what changed since the base. A
chunk with neither reads as its file's base pages, or zeros; a page of zeros
in a file without a base reads as a hole. A delta without a base is invalid.

**Chunk maps.** A file's chunks are named by a copy-on-write radix tree, as
ZFS names a file's blocks with indirect blocks: nodes of up to `fanout` (64)
entries, a leaf naming chunks' blobs, an inner node the nodes below, as few
levels as the file needs. The top node is in the root; the others are blobs.
A file of at most 64 chunks (1 MiB) is a leaf in the root, a 64 MiB file
two levels, 10 GiB four. A flush writes a new node on each path it changes
and keeps every other subtree, and a cold read fetches a level per round
trip. A subtree of holes is not stored.

**Records** are whole named values: in the root up to `inline_record_bytes`
(1 KiB), and a blob of their own when larger, written when they change.

**One rule.** A blob is put before the first root that names it, and deleted
only once no root that a reader may still hold names it. The store keeps a
prefix of the writes, so whatever a crash keeps, the root stored names only
blobs that are stored. Nothing is overwritten except the root, so there is
no log to replay, nothing to checkpoint and nothing to repair after a crash.

Exact encodings are in [FORMAT.md](FORMAT.md).

## Flush

SQL writes into a private view (dirty pages and the engine's own WAL file).
`Sql::flush(&mut wal)` checkpoints that file inside the engine and copies the
changed pages into the WAL, as writes to the file `db`; the WAL's flush then
writes every file and record in it, and syncs once:

1. **Encode.** Each chunk a write touched becomes a new delta against its
   chunk's base: the base is decoded (from the state, the cache, or the
   store) and the chunk's pages compressed with it as the reference. Once
   the delta would pass `rebase_ratio` (0.25) of the base's stored bytes,
   the chunk is written as a new base instead, and its old base and delta
   die. A record past the inline budget is compressed into a blob.
2. **Map.** Each map node on a path to a changed chunk is written anew
   (below the root's top node, a blob), and the old one dies; so does a
   node a shrinking file no longer needs.
3. **Garbage.** Delete the blobs that died `garbage_after` (16) flushes ago
   or earlier. The root lists each dead blob with the version it died at,
   so a crash between that root and the delete leaks nothing: the next flush
   deletes it.
4. **Blobs.** Put the flush's blobs, at numbers from the root's `next_blob`:
   none at or past it is named, so a crashed flush's leftovers are simply
   overwritten.
5. **Root.** Put the root, at the next version, naming them, with the
   records, and listing the blobs it names no more as garbage.
6. **Sync.** One sync stores all of it: one round trip, whatever the flush
   wrote.

**Compression.** zstd does the work twice. A chunk compresses its 4 pages
together, so a B-tree page compresses against its neighbours. And a delta
uses its base as a reference prefix ("patch from"), so a flush that changes
a few bytes of a page writes about those bytes plus the frame's overhead, and
a VM's memory is compressed against its golden image. The rebase bound keeps
a read at two blobs: the base and one delta.

**Bytes.** A flush writes the root (the directory and each file's top map
node), a node per changed path and level below it, and a delta per changed
chunk. A node is a few bytes per entry before compression: blob numbers as
varint differences from the one before. `Ok(FlushReport)` means
flushed; an error leaves the outcome unknown.

**Effects.** A host may flush bytes of its own with a transaction, as a
record (the SDKs' `effects`): set in the same WAL, they ride in the same
root put, so they are durable exactly when the transaction is, and stay
until a later flush replaces them. The Restate SDKs keep a transaction's
sends there, as its outbox, and make them once it is flushed. A host may set
any record (`State::record`) and write paged files of its own
(`State::file`) in a WAL, made durable by its flush. A WAL that changes
nothing writes nothing.

## When a flush happens

At `Wal::flush`, and only then. SQL statements and SQL `COMMIT` change the
engine's private view; `Sql::flush(&mut wal)` commits an open SQL transaction
and copies what the SQL committed into the WAL, as writes to the file `db`;
the WAL's flush makes it durable with every record and file in the WAL, in
one root, and consumes the WAL. SQL keeps the copied pages in its view until
the WAL's receipt says the state flushed it: a WAL dropped unflushed discards
its writes, and SQL's go into the next one. A flush is refused while SQL
holds changes committed but not yet moved into a WAL.
`Sql::transaction(closure)` wraps `BEGIN`/`COMMIT`, and rolls back when the
closure returns `Err`.

A continuing session moves its view forward locally instead of reopening:
the flush's root and blobs become the state's. Each flush costs one round
trip. Flushes are sequential: a second flush of the state is refused while
one runs, and each builds on the root of the one before, once its sync
returned.

**Retries without a journal.** With at-least-once delivery, a handler that
fails after a flush runs again on top of it. Two patterns keep that correct:

- **Idempotent requests.** In the same SQL transaction as the request's
  effects, write its id, and stop if it was already there:
  a plain `INSERT` into a table keyed by the id fails on a duplicate, which
  rolls back `Sql::transaction`, or `ON CONFLICT DO NOTHING RETURNING id`
  returns no row for one. The dedupe row and the effects become durable
  together, because they are one root.
- **Outbox for calls.** Flush the intent to call another object or service
  with the transaction, as effects, then make the call, and drop it from a
  later flush once the call is stored. A retry finds it and calls again, so
  the callee must deduplicate too.

A flush that returned an error has an unknown outcome: reopen and look (the
dedupe row says whether it landed) rather than retrying blindly.

**Choosing.** A flush per session costs one round trip per session and keeps
a failed attempt invisible; a flush per SQL transaction costs one round trip
per write transaction and makes progress durable as it happens. With a
journal, prefer one per session: a replay re-runs the SQL against the state
it started from. Without one, a flush per transaction is the only choice that
does not lose acknowledged work when a later step fails.

## Open

1. **Get the root.** An owner without a cache, or whose last flush had an
   unknown outcome, syncs in the same round trip, so that what it reads is
   durable.
2. **Get what it needs at once,** in one more round trip: the blobs of the
   large records, and every chunk of a file of at most `prefetch_pages`
   (16) pages, such as a small database whole (its map is in the root). A
   cached owner gets only those the cache lacks.

There is nothing to confirm, replay or repair: the root read names only
stored blobs, and whatever a crashed flush left (blobs no root names) is
overwritten by the next flush.

## Readers

A reader (`State::open(store).reader()`) reads a namespace that another owns,
alongside it, such as a virtual object's shared handler beside the exclusive
one. It writes nothing to the store, nor to the cache: its SQL changes stay
in the session, and its WALs refuse to flush. It reads the root it opened,
and the blobs that root names never change, so it reads one consistent
state, however the owner flushes meanwhile: snapshot isolation, with no
check and no retry. The owner deletes a blob `garbage_after` (16) of its
flushes after the last root that named it, so a reader that outlives that
many flushes fails with "is gone", and its handler is retried. With the
cache, a reader reads the blobs the owner cached.

## Failures

A crash anywhere keeps a prefix of the writes since the last sync.

| Crash after               | State afterwards                            | Recovery                                |
| ------------------------- | ------------------------------------------- | --------------------------------------- |
| Some garbage deletes      | The root before, which no longer names them | The next flush deletes the rest         |
| Some blobs, before a root | Blobs at numbers no root names              | Ignored; the next flush overwrites them |
| The root                  | The new root, with every blob it names      | Open reads it                           |

## Engine integration

How the [`sql`](../../sql/README.md) crate runs the SQL engine on the state.
The engine sees two virtual files, the database and its WAL file (SQLite's
own), served by `KvIo`:

- **Reads** of the database file go to the committed view: the chunk's
  decoded image, else its blobs from the cache, else from the store. The
  engine issues one dependent read at a time; adjacent queued reads are
  batched (up to 16) without crossing write, truncate or sync barriers. A get
  fetches a whole chunk, and after two consecutive chunk misses each further
  one also fetches the following chunks, in a window that doubles up to 16,
  so a scan costs a few round trips rather than one per chunk.
- **Writes, truncation and sync** stay in private memory. File sync is only an
  ordering barrier. Nothing reaches KV before a WAL's flush, and the engine's
  WAL file never does.
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
  from a flush leaves its outcome unknown.
  `Sql::is_usable` says which happened.
- **Flush.** `Sql::flush(&mut wal)` commits an open SQL transaction and copies
  the pages SQL committed into the WAL, as writes to the file `db`, and takes
  the WAL's receipt. The pages stay in SQL's private view until the receipt
  says the state flushed that WAL; a WAL dropped unflushed discards its copy,
  and the next `flush` copies them again. Nothing is durable before a WAL's
  flush. Inside `Sql::transaction`, a statement that ends the transaction
  (`COMMIT`, `END`, `ROLLBACK`) fails and ends the session before anything is
  durable, and so does dropping or unwinding the transaction future.

## Owner cache

One `PageCache` per process. A state opens in it under its namespace's key
(`State::open(store).cached(&cache, key)`), and keeps that namespace's part of
it (a `NamespaceCache`, inside the crate).

**Blobs.** A blob never changes once a durable root names it, so the cache
keeps it under (namespace, database, number) in memory, spilling to a local disk file
([foyer](https://github.com/foyer-rs/foyer)), one budget for all namespaces,
for as long as it likes. There is nothing to invalidate: no versions, no
generations, no epochs, no read marks. The one care is what goes in: a blob
is cached only once a durable root names it (after this owner's flush
synced, or when the owner read it as named by the root it opened), so a
crashed flush's blobs, which the next flush overwrites, are never cached.
The database is the root's random identity, drawn when its first root was
made: a namespace emptied (its object's state cleared) and made again
numbers its blobs from 0 again, under another identity, so it never reads
the old database's blobs. Readers never insert. A memory hit is served without foyer's disk hop, and
concurrent reads of one blob share one get, held weakly, so a cancelled
read cancels the get.

**Per-namespace state:** the root bytes this owner last read or wrote, the
version of its last durable flush, and what participants left idle there (a
parked engine). Open compares the root it reads with the one it remembers:
the same bytes, and this owner continues as it left, with its parked engine;
any other root, and it reads on from the cache by blob number, which is
still right, and drops only the idle state. A flush whose outcome is unknown
forgets the root, and the next open syncs first.

**States of one owner.** States may overlap, such as a writer and readers
for a virtual object's shared handlers. A flush is refused when another
state flushed since this one opened, rather than replacing the root that
state wrote. Each reads its own root.

**Engine reuse.** A session that ends cleanly (after a successful flush, or
read-only) parks its engine, but only if that engine holds the owner's latest
durable flush (an overlapping state's engine may be older) and its SQL
changed no per-connection state (`PRAGMA`, `ATTACH`, `DETACH`, the temp schema).
Only the database is kept: the next session connects afresh, so connection
counters (`last_insert_rowid()`, `changes()`) and settings never carry over.
This keeps the parsed schema warm, which cuts CPU per session several-fold
([cache results](../../sql/benches/CACHE_RESULTS.md)). Parked engines share
a byte budget (`PageCacheConfig::idle_bytes`, 64 MiB), each weighed as 4 MiB
(turso's buffer pool) plus 16 bytes per byte of schema SQL. Namespace states
are capped by count. Least recently used go first; dropping either only makes
a namespace cold.

**Threads.** foyer's disk I/O runs on the host's tokio runtime, or on a
one-thread runtime of its own. It is the only background work.

## Costs

| Operation        | Round trips                                                                                                            | Mutations                                                                |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| Flush, cached    | 1, the sync                                                                                                            | A delta (or base) per changed chunk, the root, the old garbage's deletes |
| Flush, cold      | 1, plus 1 to get the bases of the chunks it changes, plus 1 per map level below the top                                | The same, plus a map node per changed path and level                     |
| Open, with cache | 1, plus 1 for large records' blobs the cache lacks                                                                     | None                                                                     |
| Cold open        | 2: the root, then a small database whole and the large records                                                         | None                                                                     |
| Cold page read   | 1 get of the chunk's base and delta, after 1 per map level below the top; sequential reads fetch up to 16 chunks ahead | None                                                                     |

Measurements: [the experiment](COW_EXPERIMENT.md),
[cache](../../sql/benches/CACHE_RESULTS.md).

## Limits and open questions

- **Size.** A paged file holds at most 10 GiB (655,360 chunks, four map
  levels); the SQL database is capped there too (`PRAGMA max_page_count`),
  so a statement that would grow it past fails with SQLITE_FULL and the
  session goes on. A flush costs a node per map level on each changed path,
  so a large database pays a few hundred bytes more per flush, and a cold
  read a round trip per level; a small one, a leaf in the root, neither.
- **Keys.** Every flush that changes pages puts at least two keys (a blob and
  the root), and a namespace holds a key per live blob, about two per
  chunk, plus `garbage_after` flushes' garbage.
- **Fencing.** Restate provides it: a former attempt's writes never land
  after a newer owner's, so the state adds none.
- **Memory.** A session's private changes (`max_private_bytes`, 256 MiB) and a
  statement's result rows (`max_result_bytes`, 64 MiB) are capped; exceeding
  either fails the statement. A state keeps at most 1,024 decoded chunk
  images. Parked engines share a byte budget, by a measured estimate.
- **Integrity.** Restate provides it: a value reads back as written. Decoding
  checks structure and bounds versions, blob numbers, sizes and counts, and
  every zstd frame carries a checksum, which also catches a delta decoded
  against the wrong base.
- **Format.** Only v16, with no migration.
- **Validation.** A monitor checks every write of the simulator's campaigns
  against the one rule, decoding every root and blob it sees: a blob put
  below the root's next number (an overwrite of one a root may name), a root
  that names a blob not stored or does not build on the root before, a
  delete of a blob the root does not list as garbage, or a chunk or record
  that does not decode. The campaigns' readers check the grace. A seeded random campaign over the public API (`tests/campaign.rs`;
  `STATE_SEEDS=<n>` runs more seeds) mixes changes to records and files,
  faults, cancellations, crashes, reopens, owners and readers, and compares
  every reopen and every read with a model; other tests drop a flush after
  every number of polls, corrupt every stored value, and fuzz the decoders.
  The Restate crash tests kill processes; there are no power-loss tests.

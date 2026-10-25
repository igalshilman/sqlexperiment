# Design

Transactional state over a Restate virtual object's state (a key/value
namespace with one owner at a time): records and paged files, changed through
a mutation, which a flush makes durable together, atomically. What writes it
keeps changes of its own and moves them into the mutation before the flush; the
main writer is SQLite-compatible SQL (the [`sql`](../../sql/README.md) crate: Turso's
`turso_core` engine, unmodified) on the file `db`.

Its host is an object without Restate's journal: a key, its own database,
and calls to other objects or services, with at-least-once delivery. The
namespace is all the object keeps, and the state is how it reaches the store:
the host flushes a mutation (records, paged files and the SQL's file `db`, in one
put) when it chooses, after each SQL transaction as SQLite does, or once per
turn of everything that takes part. The sections on SQL describe the `sql`
crate, which keeps its engine's state private until it flushes into a mutation.

## Constraints

- **The backend is a Restate object's state** ([`kv`](../src/kv.rs)):
  whole-value `get`, and `put` and `delete` of one key, atomic, queued in
  issue order; `sync` returns once every write before it is stored; `keys`
  lists every key (Restate's `getKeys()`), to collect garbage. No multi-key
  transactions or compare-and-swap.
- **A crash keeps a prefix.** Of the writes since the last sync, a crash (or
  a failed sync) keeps the first few and loses the rest: never a write
  without the ones before it. Values are never torn. Reads see every write
  this owner issued.
- **One owner at a time.** Restate runs one exclusive handler per object key,
  and fences a former attempt's writes. This crate does not fence. Packs
  kept beside Restate, in S3 ([`ObjectStoreBlobs`](../src/blobs.rs)), are
  not fenced: there every pack has a name no other put uses (below).
- **Round trips dominate.** A flush or open should cost one round trip in
  the steady state.

## The model

A namespace holds these keys:

| Key                            | Holds                                                                                                                     |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------- |
| `v19/head`                     | The head, the commit point: the version, the frame size, the checkpoint and the one before, and the tail entries kept.    |
| `v19/tail/<version>/<part>`    | The tail: one entry per flush, the mutation it made, in parts of at most 1 MiB.                                           |
| `checkpoint/<version>-<nonce>` | A pack, put once and never changed: a checkpoint's frames, patches and large values, then its manifest. In a `BlobStore`. |

Packs are kept in Restate by default ([`KvBlobs`](../src/blobs.rs), in
parts of 64 KiB under `v19/pack/`), or in S3.

**Frames.** A paged file is a row of frames of `frame_pages` pages (4,
16 KiB; set when the namespace is made, and kept in the head), as many as
its size needs, at most `MAX_FILE_BYTES` (10 GiB). A frame is stored as one
zstd frame, its pages compressed against its file's base pages if it has
one (a VM's memory over its golden image) or against nothing, so a read
fetches one range of one pack. A frame of only its base's pages, or zeros,
is not stored; a page of zeros where the file has no base page reads as a
hole.

**The tail.** A flush stores what its mutation changed, as it changed it:
the records set, the log entries appended and truncations, the files'
sizes, and each page written, a zstd frame of its own compressed against
its *reference*: the page as the head's checkpoint holds it (its patch,
else its frame), else its base's page. The reference stays the same for
the whole tail, so an entry decodes alone, and a small change to a page
costs bytes in proportion to what changed since the checkpoint. Applied in
memory, the tail is the *overlay*: the pages written since the checkpoint,
the records and logs as they are, and, for a file that shrank, where it was
cut.

**Checkpoints.** A checkpoint is the state as of a version, in one pack,
with a *manifest* that says where every frame, patch, large record and log
entry is: in this pack, or in an older one. A *patch* is one page changed
since its frame was written, compressed against the frame's page. A head
names its checkpoint and the one before (for readers that opened on it),
and keeps the tail since the one before.

**Records** are whole named values: in the manifest up to
`inline_record_bytes` (1 KiB), and a value of their own in a pack when
larger, written by the first checkpoint after they change.

**Logs** are named sequences of entries, at the next index: a mutation
appends to a log and truncates it from its start (`Mutation::append`,
`Mutation::truncate`), at most 4096 entries held. A flush writes what it
appends, not the log, and a checkpoint writes each entry once: a log is for
changes too small to rewrite their whole each time, folded into a record or
a file now and then and truncated. A truncation applies to the log as the
flush leaves it, whatever its order among the mutation's appends; an entry
appended and dropped by one flush is never stored.

**One rule.** A tail entry or a pack is stored before the first head that
names it, and dropped only once no head that a state may still hold needs
it. The store keeps a prefix of the writes, so whatever a crash keeps, the
head stored names only what is stored. Nothing a head names is ever
overwritten, so there is nothing to repair after a crash.

Exact encodings are in [FORMAT.md](FORMAT.md); the experiment that made
this layout, with measurements, is [FRAMES.md](FRAMES.md).

## Flush

SQL writes into a private view (dirty pages and the engine's own WAL file).
`Sql::flush(&mut mutation)` checkpoints that file inside the engine and copies the
changed pages into the mutation, as writes to the file `db`; the mutation's flush then
writes every file and record in it, and syncs once:

1. **Checkpoint**, if one is due (below): the pack is put first, and the
   entry builds on it.
2. **Entry.** Each page written is compressed against its reference (from
   the state's decoded frames, the cache, or the store). The records, the
   log changes and the files' sizes go in as they are. An entry past 1 MiB
   is cut into parts.
3. **Put** the entry's parts at the next version, then the head, naming
   them; a flush that adopts a checkpoint also deletes the tail that the
   checkpoint before covered.
4. **Sync.** One sync stores all of it: one round trip, whatever the flush
   wrote. Once the head is stored, the transaction is durable; before, it
   doesn't exist.
5. **Apply** the entry to the overlay, in memory: the state goes on from it.

**Compression.** zstd does the work twice. A frame compresses its 4 pages
together, so a B-tree page compresses against its neighbours, and against
its file's base pages. And a page in the tail, or a patch, uses the page as
the checkpoint holds it as a reference prefix ("patch from"), so a flush
that changes a few bytes of a page writes about those bytes plus the
frame's overhead.

**Bytes.** A flush writes its entry (each page written, each record set,
each entry appended) and the head (a few dozen bytes, and a varint per
tail entry kept). `Ok(FlushReport)` means flushed, and says what it wrote;
an error leaves the outcome unknown.

**Effects.** A host may flush bytes of its own with a transaction, as a
record (the SDKs' `effects`): set in the same mutation, they ride in the same
tail entry, so they are durable exactly when the transaction is, and stay
until a later flush replaces them. The Restate SDKs keep a transaction's
sends there, as its outbox, and make them once it is flushed. A host may set
any record (`State::record`) and write paged files of its own
(`State::file`) in a mutation, made durable by its flush. A mutation that changes
nothing writes nothing.

## Checkpoint

A flush that writes anything checkpoints first once the tail holds
`checkpoint_entries` (128) entries since the head's checkpoint, or
`checkpoint_bytes` (1 MiB); with packs in Restate, where a checkpoint costs
only its bytes, once it holds the bytes the checkpoint's packs hold, if
fewer, so a small database's tail stays short. A mutation marked with
`Mutation::checkpoint` checkpoints however short the tail is. A checkpoint
writes the state as of the head, into one new pack:

1. **Frames.** A frame the overlay changed only a few pages of is
   *patched*: each page it changed is put as a patch, compressed against
   the frame's page, while the frame's patches stay within half its stored
   bytes. Any other frame the overlay touched (or a cut reached) is rebuilt
   from its old frame and patches (fetched, in parallel) and the overlay's
   pages, and compressed again, without patches.
2. **Values.** Records set since, and log entries appended since: inline
   in the manifest if small, else a zstd frame of their own.
3. **Compaction.** A pack that the manifest needs less than half of has
   its live ranges copied, raw, into the new pack.
4. **Put** the pack: frames, patches, values, copies, then the manifest.
   One put.

The flush's head then names it, and the head's checkpoint becomes the
previous one. A checkpoint that fails fails nothing: the flush goes on
without it, the tail grows, and the next flush tries again. A file made
with a base needs it to rebuild its frames, so it takes no write unless it
is opened with that base.

## When a flush happens

At `Mutation::flush`, and only then. SQL statements and SQL `COMMIT` change the
engine's private view; `Sql::flush(&mut mutation)` commits an open SQL transaction
and copies what the SQL committed into the mutation, as writes to the file `db`;
the mutation's flush makes it durable with every record and file in the mutation, in
one tail entry, and consumes the mutation. SQL keeps the copied pages in its view until
the mutation's receipt says the state flushed it: a mutation dropped unflushed discards
its writes, and SQL's go into the next one. A mutation flushed without
`Sql::flush` leaves SQL's changes private, for a later one: pairing the two
is the host's.
`Sql::transaction(closure)` wraps `BEGIN`/`COMMIT`, and rolls back when the
closure returns `Err`.

A continuing session moves its view forward locally instead of reopening:
the flush's entry joins the state's overlay. Each flush costs one round
trip. Flushes are sequential: a second flush of the state is refused while
one runs, and each builds on the head of the one before, once its sync
returned.

**Retries without a journal.** With at-least-once delivery, a handler that
fails after a flush runs again on top of it. Two patterns keep that correct:

- **Idempotent requests.** In the same SQL transaction as the request's
  effects, write its id, and stop if it was already there:
  a plain `INSERT` into a table keyed by the id fails on a duplicate, which
  rolls back `Sql::transaction`, or `ON CONFLICT DO NOTHING RETURNING id`
  returns no row for one. The dedupe row and the effects become durable
  together, because they are one tail entry.
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

1. **Get the head.** An owner without a cache, or whose last flush had an
   unknown outcome, syncs in the same round trip, so that what it reads is
   durable. An owner whose cache holds the state as of this head reuses it,
   and reads nothing more.
2. **Get the checkpoint's manifest and the tail past it,** together, and
   apply the tail in memory.
3. **Get what the manifest stores of the records and logs,** and every
   frame and patch of each file of at most `prefetch_pages` (16) pages
   without a base, such as a small database whole, together. A cached
   owner gets only those the cache lacks.

There is nothing to confirm, replay against the store or repair: the head
read names only what is stored, and whatever a crashed flush left (a tail
entry past the head's version, a pack no head names) is overwritten by the
next flush, or collected.

## Garbage

The tail goes with the checkpoints: a flush that adopts a checkpoint deletes
the tail entries the checkpoint before covered, in its own sync. Those since
then stay for readers that opened on that one.

Packs are collected (`State::collect_garbage`, and after every flush that
checkpoints):

1. **List** the packs: S3's list, or, in Restate, the store's keys
   (`KvStore::keys`: Restate's `getKeys()`). A store that cannot list
   keeps its garbage.
2. **Keep** what the head's checkpoints need: the packs its checkpoint's and
   previous checkpoint's manifests name.
3. **Delete** every listed pack older than the head's previous checkpoint
   that neither names.

A later head names only packs its head's checkpoints name, or newer than
its previous checkpoint, so whatever head a collection read (a reader's, a
zombie's, an older one), it never deletes what a later head needs.
Collections may overlap, crash or run beside the owner; the worst is a
delete of a pack already gone. A collection that fails, or is dropped
midway, ends its state. A crashed attempt's or a zombie's packs, which no
head names, are deleted once the head's previous checkpoint passes them.

## Packs beside Restate

Packs in S3 are not fenced: a former attempt that Restate fenced from the
head may still be uploading one, and an upload may land after its attempt
failed. So a pack's name is its version and a random nonce: no two puts
ever use one name, and no head names a pack a zombie puts, nor any before
it is stored, as a checkpoint is uploaded before its flush puts the head.
[`ObjectStoreBlobs`](../src/blobs.rs) keeps them under the namespace's
prefix; a range read is one ranged GET.

Kept in Restate ([`KvBlobs`](../src/blobs.rs), the default), a pack is cut
into parts of 64 KiB, which a put only queues: the flush's sync stores them
with its tail entry, before the head that names them, as the store keeps
writes in order. A range read gets the parts it overlaps.

## Readers

A reader (`State::open(store).reader()`) reads a namespace that another owns,
alongside it, such as a virtual object's shared handler beside the exclusive
one. It flushes nothing: its SQL changes stay in the session, and its
mutations refuse to flush. It reads the head it opened, its
checkpoint and its tail, and packs never change, so it reads one consistent
state, however the owner flushes meanwhile: snapshot isolation, with no
check and no retry. The packs and the tail its head names stay until the
owner's second checkpoint after it, so a reader that outlives that fails
with "is gone", and its handler is retried. A reader may collect garbage
too. With the cache, a reader reads the ranges the owner cached.

## Failures

A crash anywhere keeps a prefix of the writes since the last sync.

| Crash after                             | State afterwards                              | Recovery                                                |
| --------------------------------------- | --------------------------------------------- | ------------------------------------------------------- |
| Some garbage deletes                    | The head, which needs none of them            | The next collection deletes the rest                    |
| A pack, or some of its parts            | A pack no head names                          | Collected once the head's previous checkpoint passes it |
| Some of a tail entry, before a head     | An entry past the head's version              | Overwritten by the next flush                           |
| A flush's tail deletes, before its head | The head before, its tail past its checkpoint | The next checkpoint deletes the rest                    |
| The head                                | The new head, with everything it names        | Open reads it                                           |

## Engine integration

How the [`sql`](../../sql/README.md) crate runs the SQL engine on the state.
The engine sees two virtual files, the database and its WAL file (SQLite's
own), served by `KvIo`:

- **Reads** of the database file go to the committed view: the overlay's
  page, else the page's patch or frame, decoded, from the state, the cache,
  or the store. The engine issues one dependent read at a time; adjacent
  queued reads are batched (up to 16) without crossing write, truncate or
  sync barriers. A get fetches a whole frame, which the state keeps decoded,
  so its neighbours read without another.
- **Writes, truncation and sync** stay in private memory. File sync is only an
  ordering barrier. Nothing reaches KV before a mutation's flush, and the engine's
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
- **Flush.** `Sql::flush(&mut mutation)` commits an open SQL transaction and copies
  the pages SQL committed into the mutation, as writes to the file `db`, and takes
  the mutation's receipt. The pages stay in SQL's private view until the receipt
  says the state flushed that mutation; a mutation dropped unflushed discards its copy,
  and the next `flush` copies them again. Nothing is durable before a mutation's
  flush. Inside `Sql::transaction`, a statement that ends the transaction
  (`COMMIT`, `END`, `ROLLBACK`) fails and ends the session before anything is
  durable, and so does dropping or unwinding the transaction future.

## Owner cache

One `PageCache` per process. A state opens in it under its namespace's key
(`State::open(store).cached(&cache, key)`), and keeps that namespace's part of
it (a `NamespaceCache`, inside the crate).

**Packs.** A pack never changes, and its name is never reused, so the cache
keeps its ranges (frames, patches, values, manifests) under (namespace,
pack, offset) in memory, spilling to a local disk file
([foyer](https://github.com/foyer-rs/foyer)), one budget for all namespaces,
for as long as it likes. There is nothing to invalidate: no versions, no
generations, no epochs, no read marks. A range of a pack that never became
durable is harmless: no head will ever name it. The namespace is a
process-wide counter, drawn per namespace handle, and the disk tier starts
empty in every process. A memory hit is served without foyer's disk hop,
and concurrent reads of one range share one get, held weakly, so a
cancelled read cancels the get.

**Per-namespace state:** the head bytes this owner last read or wrote, the
state as of that head, the version of its last durable flush, and the values
parked there (`State::park`: an engine, say). Open compares the head it
reads with the one it remembers: the same bytes, and this owner continues
as it left, with its parked engine, reading nothing more; any other head,
and it loads that head's state, from the cache where it can, and drops the
parked values. A head holds the database's random identity, drawn when it
was made, so a namespace emptied (its object's state cleared) and made
again never matches the old one. A flush whose outcome is unknown forgets
the head, and the next open syncs first.

**States of one owner.** States may overlap, such as a writer and readers
for a virtual object's shared handlers. A flush is refused when another
state flushed since this one opened, rather than replacing the head that
state wrote. Each reads its own head.

**Engine reuse.** A session that ends cleanly (after a successful flush, or
read-only) parks its engine, but only if that engine holds the owner's latest
durable flush (an overlapping state's engine may be older) and its SQL
changed no per-connection state (`PRAGMA`, `ATTACH`, `DETACH`, the temp schema).
Only the database is kept: the next session connects afresh, so connection
counters (`last_insert_rowid()`, `changes()`) and settings never carry over.
This keeps the parsed schema warm, which cuts CPU per session several-fold
([cache results](../../sql/benches/CACHE_RESULTS.md)). Parked engines share
a byte budget (`PageCacheConfig::parked_bytes`, 64 MiB), each weighed as 4 MiB
(turso's buffer pool) plus 16 bytes per byte of schema SQL. Namespace states
are capped by count. Least recently used go first; dropping either only makes
a namespace cold.

**Threads.** foyer's disk I/O runs on the host's tokio runtime, or on a
one-thread runtime of its own. It is the only background work.

## Costs

| Operation        | Round trips                                                                                                              | Writes                             |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------ | ---------------------------------- |
| Flush, cached    | 1, the sync                                                                                                              | A tail entry (its parts), the head |
| Flush, cold      | 1, plus 1 to get the frames (or patches) of the pages it writes, where the state lacks them                              | The same                           |
| Checkpoint       | Up to 3 more, for what the state lacks: frames it rebuilds or patches, ranges it compacts; in S3, 1 PUT before the flush | A pack                             |
| Collection       | 1 to list the packs; if any is older than the previous checkpoint, 1 for both manifests, then the deletes                | The deletes                        |
| Open, with cache | 1, the head, when nothing changed since this owner's flush                                                               | None                               |
| Cold open        | Up to 3: the head; the manifest and the tail; the large records and log entries, and a small database whole              | None                               |
| Cold page read   | 1 ranged get of its frame (and its patch, together); for a page in the overlay, of its reference                         | None                               |

Measurements: [frame packs](FRAMES.md),
[cache](../../sql/benches/CACHE_RESULTS.md).

## Limits and open questions

- **Size.** A paged file holds at most 10 GiB; the SQL database is capped
  there too (`PRAGMA max_page_count`), so a statement that would grow it
  past fails with SQLITE_FULL and the session goes on. A cold open reads
  the manifest whole, a frame location per frame: a few bytes per 16 KiB.
- **Bytes.** A page in the tail is compressed alone against its reference,
  and a checkpoint rewrites whole frames, so both write more bytes than a
  delta of what changed would; compaction at half live stores up to twice
  what is live ([FRAMES.md](FRAMES.md) measures it).
- **Keys.** Every flush puts at least two keys (a tail entry and the head);
  a namespace holds a key per part of each tail entry kept, and, with packs
  in Restate, a key per 64 KiB of each pack. A collection lists every key:
  a round trip, and its deletes.
- **Fencing.** Restate provides it for the head and the tail: a former
  attempt's writes never land after a newer owner's. Packs beside Restate
  are not fenced; their unique names keep attempts apart.
- **Memory.** A session's private changes (`max_private_bytes`, 256 MiB) and a
  statement's result rows (`max_result_bytes`, 64 MiB) are capped; exceeding
  either fails the statement. A state keeps the tail since its checkpoint in
  memory, as its overlay, and at most 256 decoded frames and patches. Parked
  engines share a byte budget, by a measured estimate.
- **Integrity.** Restate provides it: a value reads back as written. Decoding
  checks structure and bounds versions, sizes, counts and names, and every
  zstd frame carries a checksum, which also catches a page decoded against
  the wrong reference.
- **Format.** Only v19, with no migration.
- **Validation.** A monitor checks every request of the simulator's
  campaigns against the one rule, decoding every head, tail entry and
  manifest it sees, and every page, frame and value they name: a head that
  names a tail entry or a checkpoint (its manifest and every range it
  names) not stored, or does not build on the head before; a tail entry
  put for any version but the next, or a pack's part put twice; a delete of
  a tail entry the head's checkpoint does not cover, or of a pack a head
  may need. A seeded random campaign over the public API
  (`tests/campaign.rs`; `STATE_SEEDS=<n>` runs more seeds) mixes changes to
  records, files and logs, random checkpoint thresholds, faults,
  cancellations, crashes, reopens, owners and readers, with packs in
  Restate (even seeds) or in an S3 that fails as S3 does (odd seeds:
  uploads lost, done with the response lost, or landing late), and
  compares every reopen and every read with a model; other tests crash at
  every write of a flush, with packs in Restate and in S3, drop a flush
  after every number of polls, and corrupt every stored value. The Restate
  crash tests kill processes; there are no power-loss tests.

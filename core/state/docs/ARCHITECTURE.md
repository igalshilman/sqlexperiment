# Architecture

How the state works, from the top down: the data model a user of the crate
sees, the state as a sequence of mutations, how that sequence is stored
(the head, the tail, two levels of packs and the manifests that index
them), and how each of those changes over time. It ends with what changes
when L1 is in S3, with measured sizes and costs, and with the layouts this
one replaced.

Two docs go further: [DESIGN.md](DESIGN.md) has the rules this one leaves
out (retries, readers, the owner cache, the SQL engine, limits), and
[FORMAT.md](FORMAT.md) the byte encodings.

## 1. The data model

**A namespace** is the state of one Restate virtual object: one key, such
as `chat/42`. It has **one owner at a time**: the object's exclusive
handler, which Restate runs once per key and fences, so a former attempt's
writes never land after a newer one's. Shared handlers may open the same
namespace as **readers** beside the owner.

A namespace's state holds three kinds of named values:

| Kind       | What it is                                                              | Used for, say                                   |
| ---------- | ----------------------------------------------------------------------- | ----------------------------------------------- |
| **Record** | A whole value, up to 16 MiB, set and read whole                         | A request's effects; a run's progress           |
| **File**   | A paged file: 4 KiB pages, read and written at random, up to 10 GiB     | The SQL database (the file `db`); a VM's memory |
| **Log**    | Entries appended at the end and truncated from the start, read by index | A VM's memory turns; anything append-only       |

A file may have a **base**, a golden image the host holds (such as a VM's
initial memory): its pages are stored as differences from the base's.

**The state is read, never written in place.** A change goes into a
**mutation**: an owned value that collects records set, pages written,
files resized, entries appended and logs truncated, in memory. A **flush**
of the mutation makes all of it durable at once, atomically, in one round
trip to Restate, or none of it; a mutation dropped instead discards it.

```rust,ignore
let state = State::open(store).cached(&cache, "chat/42").await?;
let mut mutation = state.mutation();
mutation.set(&state.record("effects"), effects)?;
mutation.write(&state.file("db")?, 7, Some(page))?;
mutation.flush().await?; // both durable together, or neither
```

What the state guarantees:

- **Atomic.** After a crash, a flush is either all there or not at all.
- **Durable on return.** When `flush()` returns, the mutation is stored.
  When it fails, its outcome is unknown: the state ends, and the next open
  reads what is there.
- **Ordered.** Flushes are sequential; each builds on the one before.
- **Snapshots for readers.** A reader sees the state as of the flush it
  opened on, however many flushes the owner makes meanwhile.

**SQL on top.** The [SQL crate](../../sql/README.md) runs a
SQLite-compatible engine on the file `db`. Its statements change a private
view; at a flush, it moves the pages its transactions committed into the
mutation, beside whatever else the host puts there (the effects of the
request, say), so the database and the effects become durable together.
The language bindings flush after every call that leaves no SQL transaction
open.

## 2. The state over time: a sequence of mutations

The state is a sequence. Every flush that changes something makes the next
**version**: version 0 is empty, and

```
state(n) = state(n − 1) + mutation(n)
```

A chat agent's first turns, say:

| Version | Mutation (one flush)                                   | State after                             |
| ------- | ------------------------------------------------------ | --------------------------------------- |
| 1       | `CREATE TABLE messages`: pages 0–1 of `db`             | An empty table                          |
| 2       | Insert the user's message: page 1 of `db`              | 1 message                               |
| 3       | Insert the model's reply: page 1; set record `effects` | 2 messages, and the effects of the turn |
| 4       | Insert the next user's message: page 1                 | 3 messages                              |
| …       | …                                                      | …                                       |

Stored as is, that sequence is cheap to write (a flush puts only its own
mutation, often a few hundred bytes) but grows without end, and opening the
state would mean replaying all of it. Stored as whole states, opening is
cheap but every flush rewrites everything. So the state keeps both, in two
layers:

```
state(n) = checkpoint(c) + mutation(c + 1) + … + mutation(n)
           └ a snapshot ┘   └──────────── the tail ─────────┘
```

- **The tail**: the mutations since the last checkpoint, stored as they
  were. A flush appends one.
- **A checkpoint**: the whole state as of version `c`, written now and
  then by folding the tail into the checkpoint before it. Once a
  checkpoint is written, the tail before it can go. It describes the
  whole state but stores only what changed: its index is complete, its
  data is spread over its own pack and older ones (see
  [What a checkpoint holds](#what-a-checkpoint-holds)).

A checkpoint itself keeps two levels, for the same reason: **L0**, the
pages written since the last promotion, as the tail wrote them, cheap to
add to; and **L1**, the frames a promotion last rebuilt, cheap to read.
Every page in the tail and in L0 is compressed against its frame in L1,
so moving pages from the tail into L0 is a copy, and only a promotion,
now and then, decodes them.

```
page = tail ▸ L0 ▸ L1        (the first that holds it)
       └ deltas against L1 ┘
```

Everything below is how those layers are laid out, written and read.

## 3. How it is stored: the terms

```
Restate (the object's state)                           S3, or Restate without it
┌───────────────────────────────────────────────┐     ┌──────────────────────────────┐
│ v21/head          the commit point: version,  │     │ L1: l1/<v>-<nonce>           │
│                   checkpoints, tail entries   │     │   frames a promotion rebuilt,│
│ v21/tail/<v>/<p>  one per flush: its mutation │     │   large values               │
│ v21/manifest/...  one per checkpoint: where   │────▶│                              │
│                   every frame, page and value │     └──────────────────────────────┘
│                   is                          │
│ v21/pack/l0/...   L0: pages copied from the   │
│                   tail since the last         │
│                   promotion, large values     │
└───────────────────────────────────────────────┘
```

| Term                   | What it is                                                                                                                                                                                                                    |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Head**               | The commit point, one key (`v21/head`): the version, the current and previous checkpoints, and which tail entries exist. Writing it is what commits a flush.                                                                  |
| **Tail entry**         | One flush's mutation, as it was (`v21/tail/<version>/<part>`).                                                                                                                                                                |
| **Flush**              | Put a tail entry, then the head, then sync: one round trip.                                                                                                                                                                   |
| **Frame**              | The unit a file is stored in, in L1: 4 pages (16 KiB), compressed with zstd on its own. A file is a row of frames.                                                                                                            |
| **Reference**          | What a page written since the last promotion is compressed against: the page as its frame holds it (else its base's page, else nothing). The tail and L0 both use it, so a page moves from one to the other as it is.         |
| **Checkpoint**         | The whole state as of a version: a manifest that indexes all of it, and a pack of what it added. Written by a flush, when the tail is long enough, before its own tail entry. Into L0, or, now and then, a promotion into L1. |
| **L0**                 | Packs in Restate of the pages checkpoints copied from the tail since the last promotion, as the tail wrote them, and large values.                                                                                            |
| **Promotion**          | A checkpoint that rebuilds every frame L0 (or the tail) changed, into a pack in L1, and takes along what L0 and L1's sparse packs still hold: L0 is empty after it.                                                           |
| **L1**                 | Packs of frames promotions rebuilt, and large values: in S3 where the state has one, else in Restate too.                                                                                                                     |
| **Pack**               | What one checkpoint wrote, back to back. Put once, never changed; named `<version>-<nonce>` with its level, with a random nonce, so no two puts share one.                                                                    |
| **Manifest**           | One per checkpoint (`v21/manifest/<version>-<nonce>/<part>`), always in Restate: where every frame (L1), page (L0), record and log entry is. Small records are inline.                                                        |
| **Overlay**            | The tail, decoded in memory: the pages, records and logs changed since the checkpoint. A read looks here first.                                                                                                               |
| **Garbage collection** | Deleting the packs and manifests no checkpoint the head names still needs.                                                                                                                                                    |
| **Page cache**         | One per process, in memory and on local disk: the ranges of packs read, and each namespace's state as of the last head it saw.                                                                                                |

## 4. The pieces, and how they change over time

### A flush

1. **If a checkpoint is due**, write it first (below), as of the version
   before this flush.
2. **Encode the mutation** as a tail entry. Each page written is a zstd
   frame of its own, compressed against its **reference**: the page as its
   frame in L1 holds it (under the new checkpoint, if this flush wrote
   one). A row update usually changes a few bytes of a page, so its page
   costs about 100–200 bytes, not 4 KiB.
3. **Put** the tail entry, then the head; **sync**. Restate keeps writes in
   order, and a crash keeps a prefix of them, so the head is never stored
   without the tail entry (and the pack and manifest) put before it. That
   is one round trip, checkpoint or not.
4. **Apply** the entry to the overlay in memory. The state is now at the
   next version, without reading anything back.

### The head

About 80–180 bytes, uncompressed, overwritten by every flush:

- the database's random identity, drawn when it was made, and its frame size;
- the version;
- the checkpoint (its pack's version, nonce and level, and its manifest's
  length), and the previous one, which readers that opened before the
  latest checkpoint may still read;
- the first tail entry kept, the tail's bytes since the checkpoint, and the
  number of parts of each tail entry kept.

It is the only key whose write commits anything. Every other key is either
new (a tail entry for the next version) or never changed (a pack, a
manifest), so a crash midway leaves only keys no head names.

### Tail entries

One per flush, for the version it makes, cut into parts of 1 MiB if large:

- the records set, with their values;
- per log changed, the entries appended and where it was truncated (an
  entry appended and truncated in the same flush is dropped);
- per file changed, its new size and the pages written, each compressed
  against its reference.

Every page since a promotion compresses against the same frames, so each
entry decodes alone, and a checkpoint into L0 copies its pages as they
are. An entry is never changed; it is deleted when the second checkpoint
after it is adopted, until which readers on the older checkpoint may need
it.

### Checkpoints

**When.** A flush checkpoints first when, since the last checkpoint:

- 128 flushes have been made (`checkpoint_entries`), or
- the tail holds as many bytes as L0 and the manifest (but at least a
  frame's, 16 KiB, and at most 1 MiB, `checkpoint_bytes`),

or when the mutation asks for one (`mutation.checkpoint()`). A checkpoint
into L0 costs only its bytes in Restate, and the manifest it rewrites, so
a small database checkpoints every few dozen flushes and keeps a short
tail, and a 100 MiB one every 128.

**Into L0**, the usual case: the checkpoint copies each page the overlay
holds, as the tail wrote it, into a new pack in L0, and records where in
the manifest; a page the overlay holds as a hole is a hole in L0. A file
cut since the last checkpoint records its cut: its frames' pages past it
read as holes, unless L0 holds a page written after. There is nothing to
decode or read, and no choice to make per frame.

**A promotion**, now and then: once what a promotion would rebuild or
reclaim (L0, the tail it would copy, and the dead bytes of L1's packs that
are under half live) is as much as L1 holds live, but at least a frame's
bytes and at most `l0_bytes` (1 MiB), the checkpoint goes into L1 instead.
It copies the overlay into L0, in memory, as above, then **folds** L0 into
the frames: every frame with a page in L0, or past a cut, is read, its
pages decoded against it and written in, and compressed again, into a new
pack in L1. Then it **compacts**: what L0's packs and L1's packs under half
live still hold (large values, frames) moves into the new pack as it is,
L1's up to 64 MiB a promotion, the cheapest packs first. L0 is empty
after it.

A small database promotes about as often as it checkpoints, as its L1 is
small; a 100 MiB one keeps up to 1 MiB of pages in L0 and promotes every
few thousand flushes.

Then the records set and log entries appended since go in, either way:
inline in the manifest if up to 1 KiB, else into the pack. All of that is
one pack and one manifest. The flush's head then names the new
checkpoint, the one before becomes the previous checkpoint, and the tail
entries the previous checkpoint covered are deleted in the same sync.

A checkpoint that fails (an upload, say) fails nothing: the flush commits
its tail entry anyway, reports why (`FlushReport::checkpoint_failed`), and
the next flush tries again. Each failure in a row after that doubles the
flushes before the next try, up to 64, so a checkpoint that keeps failing
(its store down, say) costs one flush in 64 rather than each; one that
succeeds ends the wait. The process's cache keeps the wait for the
namespace's next states, and `Mutation::checkpoint` tries regardless.

### Manifests

One per checkpoint, zstd-compressed, always in Restate, in 64 KiB parts:

- the packs it uses, with their level, and the bytes each holds (for
  compaction);
- every record: inline, or where its value is;
- every log: its first index and where each entry is;
- every file: its size, its base, its cut, a location (pack, offset,
  length) per frame in L1, or none for a hole, and its pages in L0, each a
  location or a hole.

About 1.7 bytes per frame and 4 per page in L0: 3 KiB for a 100k-row
database, 30 KiB for 1M rows, and up to about 1 KiB per 256 pages in L0. A
checkpoint writes its manifest whole. Two are kept, the head's
checkpoint's and the previous one's.

### Packs

One per checkpoint that stores anything outside its manifest. Put once,
never changed, named with a random nonce, so a cache can keep any range of
any pack forever, with nothing to invalidate. A pack lives as long as some
manifest the head names points into it. A pack in L0 lives until the
promotion after it, and two checkpoints more; a frame in L1 no flush
touches stays in the pack that first wrote it, across any number of
promotions, until compaction moves it.

### What a checkpoint holds

A checkpoint has two parts: a manifest that indexes the whole state, and a
pack of what it added: pages copied into L0, or frames a promotion
rebuilt.

- **The manifest** names every file, record and log, and gives a location
  for every frame in L1, every page in L0 and every stored value. It
  stands on its own: an open reads this one manifest.
- **The pack** holds what the checkpoint added. For everything else, the
  manifest points into the older packs that already hold it.

Say `db` has 6 frames (f0–f5, 4 pages each), with a checkpoint every 4
flushes, and L0 promoted at the fourth checkpoint:

| Checkpoint | What the tail since the one before changed | Its pack holds                          | Level |
| ---------- | ------------------------------------------ | --------------------------------------- | ----- |
| C4         | Every frame (the database was made)        | P4: f0–f5                               | L1    |
| C8         | Page 9 (in f2); pages 20–23 (all of f5)    | Q8: p9, p20–p23, as the tail wrote them | L0    |
| C12        | Page 9 again; page 1 (in f0)               | Q12: p9, p1                             | L0    |
| C16        | Page 2 (in f0); a promotion                | P16: f0, f2, f5, rebuilt                | L1    |

**Where each manifest points.** Every manifest names every frame and every
page L0 holds, and shares what did not change with the manifests before
it:

```
          f0   f1   f2   f3   f4   f5  │ in L0
M4        P4   P4   P4   P4   P4   P4  │ –
M8        P4   P4   P4   P4   P4   P4  │ p9 Q8, p20–p23 Q8
M12       P4   P4   P4   P4   P4   P4  │ p1 Q12, p9 Q12, p20–p23 Q8
M16       P16  P4   P16  P4   P4   P16 │ –
```

The frames do not change between promotions: M8 and M12 share all of P4,
and add pages. A read of page 9 under M12 takes p9 from Q12 and decodes
it against page 9 as f2 holds it, in P4; a read of page 8 takes f2. Under
M16, f2 is rebuilt with page 9 in it, and L0 is empty.

**How long each pack lives.** Garbage collection keeps what the head's two
manifests, its checkpoint's and the previous one's, point into, and
deletes older packs that neither names:

```
         C4     C8     C12    C16    C20
P4       ●━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━▶   M16 still reads f1, f3 and f4 from it
Q8              ●━━━━━━━━━━━━━━━━━━━━━×        M12 points into it; M16 and M20 do not
Q12                    ●━━━━━━━━━━━━━━×        the same
P16                           ●━━━━━━━━━━▶

● written    × deleted, two checkpoints after the last manifest that named it
```

- **Sharing:** a checkpoint writes what the tail changed; every frame it
  did not touch stays where a promotion wrote it, however long ago.
- **One reference:** every page in the tail and in L0 is a delta against
  its frame in L1, so a page reads from at most one page in L0 and one
  frame, and a checkpoint into L0 is a copy.
- **Folding:** a promotion folds L0 and the tail into the frames they
  changed (f0, f2 and f5 at C16); L0's packs are then garbage.
- **Compaction:** at a promotion, an L1 pack the new manifest needs less
  than half of has its live ranges copied, as they are, into the new pack
  (up to 64 MiB a promotion), so what L1 stores stays within about twice
  the state. P4 is still half live after C16, so it stays.
- **Size:** a checkpoint into L0 is the pages the tail wrote, as it wrote
  them; a promotion, the frames L0 changed; the manifest is the part that
  grows with the database.

### A timeline

With a checkpoint every 4 flushes (128 by default), `E5` the tail entry of
version 5, and `C4` the checkpoint of version 4 (its pack and manifest
`M4`):

| Flush makes | Checkpoint first | The head names   | Tail kept | Deleted in the same sync | Garbage collected after                    |
| ----------- | ---------------- | ---------------- | --------- | ------------------------ | ------------------------------------------ |
| v1 … v4     | –                | no checkpoint    | E1 … E4   | –                        | –                                          |
| v5          | C4, a promotion  | C4               | E1 … E5   | –                        | –                                          |
| v6 … v8     | –                | C4               | E1 … E8   | –                        | –                                          |
| v9          | C8, into L0      | C8, previous C4  | E5 … E9   | E1 … E4                  | nothing older than C4                      |
| v10 … v12   | –                | C8, previous C4  | E5 … E12  | –                        | –                                          |
| v13         | C12, into L0     | C12, previous C8 | E9 … E13  | E5 … E8                  | M4; no pack, as M8 and M12 still need them |

- **State v10** is `C8 + E9 + E10`, and E9 and E10 compress against C4's
  frames, as C8's pages do.
- **A reader** that opened on v6 reads `C4 + E5 + E6`. Those stay until
  v13, two checkpoints later; after that it fails with "is gone", and its
  handler is retried.
- **In Restate**, at any time: the head, the tail since the previous
  checkpoint (at most twice 128 entries), two manifests, L0 (at most
  `l0_bytes` of pages since the last promotion, and the packs of the one
  before, until two checkpoints after it), and L1 if there is no S3.

### Opening and reading

**Open**:

1. **Get the head.** If this process's cache holds the state as of exactly
   this head (it was the last to flush, say), reuse it, with its parked SQL
   engine: done, in one round trip. An owner without a cache, or whose
   last flush had an unknown outcome, syncs in the same round trip, so
   that what it reads is durable.
2. Otherwise, **get the checkpoint's manifest and the tail past it**,
   together, and apply the tail to the overlay in memory.
3. **Get large records and log entries**, and every frame and L0 page of a
   file without a base of at most 16 pages, such as a small database
   whole, together.

That is at most 3 round trips, whatever the database's size.

**Read a page** from the first of:

1. the overlay (the tail changed it);
2. L0, if it holds the page (or a hole);
3. a hole, if the page is past its file's cut;
4. its frame in L1: one ranged read of the pack (in Restate, the 64 KiB
   parts it is in, which the cache keeps for the frames beside it; in S3,
   a ranged GET), decompressed in about 25 µs;
5. a hole: zeros, or the base's page.

A page from the overlay or L0 decodes against its frame's page, which is
fetched alongside, in the same round trip. A SQL query reads a page per
b-tree level it descends, so a cold point query is a round trip per level
the cache does not hold: 2 to 6.

### Garbage collection

After every flush that checkpoints, and whenever anyone calls
`State::collect_garbage` (a reader may too):

1. **List** Restate's keys, once (Restate's `getKeys()`): the manifests and
   L0, and L1 without S3. With S3, list it too, but only if the head's
   checkpoint or the previous one is a promotion, as only a promotion
   changes what L1 needs (or when asked, by `State::collect_garbage`). A
   store that cannot list keeps its garbage.
2. **Keep** the packs the head's two manifests point into, and the two
   manifests.
3. **Delete** listed packs and manifests older than the previous
   checkpoint that neither manifest needs: in Restate, queued and stored by
   one sync; in S3, one DELETE per pack, sent together.

Collections may overlap, crash or race the owner: a later head only ever
names packs its own manifests name, or packs newer than its previous
checkpoint, so whatever head a collection read, it never deletes what a
later head needs. A pack a crashed attempt put, which no head names, goes
once the previous checkpoint passes it (in S3, at the next promotion's
collection).

### The cache

One `PageCache` per process ([foyer](https://github.com/foyer-rs/foyer),
in memory and spilling to a local disk file):

- **Pack ranges**: frames, L0 pages, values and manifests, and the 64 KiB
  parts read of packs in Restate, kept by (pack, offset, length). Packs
  never change, so nothing is ever invalidated.
- **Namespace states**: per namespace, the head bytes it last read or
  wrote and the state as of that head, kept by their bytes (256 MiB by
  default). An open that reads the same head bytes reuses the state and
  reads nothing more; any other head (another process flushed) loads that
  head's state, from cached ranges where it can.
- **Parked engines**: the SQL crate parks its engine at the end of a
  session (64 MiB in all), so the next session skips parsing the schema.

With a warm cache, a handler invocation costs two round trips: get the
head, and the flush's sync.

### Failures

A crash anywhere keeps a prefix of the writes since the last sync, and
nothing needs repair:

| Interrupted                             | What is left                             | What happens next                                        |
| --------------------------------------- | ---------------------------------------- | -------------------------------------------------------- |
| A flush                                 | Its tail entry without the head, or both | The next flush overwrites the tail entry                 |
| A checkpoint, or a promotion            | A pack and manifest no head names        | Garbage, collected later; L0 as it was, for the next try |
| A collection                            | Some deletes                             | The next collection deletes the rest                     |
| A flush's tail deletes, before its head | The head before, with its tail           | The next checkpoint deletes the rest                     |
| An unknown outcome                      | The flush may or may not be stored       | The state ends; the next open syncs first                |

## 5. With L1 in S3

`State::open(store).checkpoints(packs)` puts L1 in an object store. Only
L1 moves. The head, the tail, the manifests and L0 stay in Restate, so an
open never needs S3 to find anything, and a checkpoint into L0 makes no
request to it.

What changes:

- **A promotion uploads its pack** (one PUT) before the flush puts its
  head, so a promoting flush also waits for S3: about one checkpoint in
  two for a small database, and one flush in a few thousand for a large
  one.
- **A frame read is a ranged GET**, about 20 ms against about 2 ms for
  Restate. A page in L0 needs its frame too, fetched alongside.
- **No fencing in S3.** Restate fences the head and the tail; S3 fences
  nothing, so a former attempt may still be uploading. A pack's name has a
  random nonce, so no two uploads ever share a name, and a head only names
  a pack its own attempt uploaded before putting it: a zombie's packs are
  garbage, collected later.
- **Garbage collection** lists S3 only around a promotion, and deletes in
  one Restate sync and one S3 DELETE per pack, sent together.

## 6. Numbers

Bytes and request counts below come from the `flush_costs` bench
(`core/sql/benches`), which runs the real encoding over in-memory stores,
with the default `l0_bytes` (1 MiB). Latencies come from `read_latency`, a
discrete-event simulation that **assumes** 2 ms per Restate round trip and
20 ms per S3 request; they show the shape, not production timings. v20
(frames and per-page patches, one level) is the layout this one replaced,
measured with the same benches on the same commit's workloads.

### L1 in Restate (the default)

Per flush, with a warm cache:

| Workload            | Database | Bytes put to Restate (tail + pack + manifest + head) | v20  | Stored in Restate | Keys  |
| ------------------- | -------- | ---------------------------------------------------- | ---- | ----------------- | ----- |
| Chat with effects   | 180 KiB  | 2276 (1645 + 516 + 51 + 64)                          | 1949 | 107 KiB           | 24    |
| Small, update 1     | 20 KiB   | 713 (489 + 126 + 2 + 96)                             | 676  | 32 KiB            | 60    |
| 10k rows, update 1  | 2.8 MiB  | 646                                                  | 639  | 1.2 MiB           | 163   |
| 10k rows, update 10 | 2.8 MiB  | 3317 (2665 + 482 + 81 + 89)                          | 2032 | 1.4 MiB           | 86    |
| 1M rows, update 1   | 274 MiB  | 857 (226 + 203 + 249 + 179)                          | 872  | 108 MiB           | 1,870 |
| 1M rows, update 10  | 274 MiB  | 1356                                                 | 1658 | 108 MiB           | 1,946 |
| 100k rows, aged     | 27 MiB   | 2872 (250 + 2274 + 108 + 240)                        | 1172 | 11 MiB            | 340   |

Most workloads cost what v20 did. Two cost more, for the two reasons the
one reference rule has a price:

- **A page changed many times between promotions** costs what changed
  since its frame, not since its last version: ten rows a flush on 10k
  rows put 2665 tail bytes a flush against 1610.
- **A promotion rewrites every frame L0 touched.** On an aged database,
  one-row updates spread over every frame: by the time L0 holds 1 MiB of
  pages, a promotion rewrites most of the database. v20's patches rewrote
  a frame only once its patches reached half of it. The cost falls as L0
  grows, and stays above v20's: 17,395 bytes a flush at 64 KiB, 8,968 at
  256 KiB, 2,872 at 1 MiB, 1,782 at 4 MiB, against 1,172.

A cold open and point read costs the same round trips as v20 (4 to 6),
and about as many gets: it reads the tail since the last checkpoint, one
get an entry, which depends on where in the checkpoint's cycle the open
falls (23 gets against v20's 50 at 300 flushes of the 100k-row database).

Cold reads, at 2 ms a round trip, the same as v20:

| Database | Open | Point query (primary key) | Point query (index) | 100-row range |
| -------- | ---- | ------------------------- | ------------------- | ------------- |
| 100 rows | 6 ms | 0 (all in memory)         | 0                   | 0             |
| 10k rows | 6 ms | 4 ms                      | 6 ms                | 8 ms          |
| 1M rows  | 6 ms | 4 ms                      | 8 ms                | 8 ms          |

### L1 in S3

| Workload            | Bytes put to Restate per flush | v20  | Stored in Restate | Stored in S3 | S3 Standard $ per million flushes, warm / cold | v20, warm |
| ------------------- | ------------------------------ | ---- | ----------------- | ------------ | ---------------------------------------------- | --------- |
| Chat with effects   | 1915                           | 1938 | 33 KiB            | 0.07 MiB     | $0.43 / $1.13                                  | $0.07     |
| Small, update 1     | 586                            | 1734 | 23 KiB            | 0.01 MiB     | $0.28 / $1.06                                  | $0.07     |
| 10k rows, update 1  | 644                            | 436  | 120 KiB           | 1.07 MiB     | $0.02 / $1.56                                  | $0.08     |
| 10k rows, update 10 | 3318                           | 1813 | 370 KiB           | 1.07 MiB     | $0.02 / $8.25                                  | $0.08     |
| 1M rows, update 1   | 858                            | 654  | 144 KiB           | 108 MiB      | $0.02 / $1.99                                  | $0.08     |
| 1M rows, update 10  | 1355                           | 1078 | 272 KiB           | 108 MiB      | $0.02 / $15.57                                 | $0.08     |
| 100k rows, aged     | 816                            | 702  | 584 KiB           | 11 MiB       | $0.00 / $2.07                                  | $0.08     |

Here a promotion's rewrite goes to S3, where bytes are almost free: the
aged database costs 16% more Restate bytes than v20, and a promotion an
upload in about 5,000 flushes. A large database checkpoints into L0 with
no S3 request, so a warm flush costs a quarter of v20's; a small one
promotes about every other checkpoint, and costs four times as much, still
cents per million flushes. Small databases put a third of v20's bytes to
Restate, as their tail is cut by the same rule as without S3.

Cold reads, at 2 ms a Restate round trip and 20 ms an S3 GET, are v20's:
24 ms to open, 40 ms for a point query by key, 60 to 80 ms by index or for
a range of 100 rows. A page in L0 adds no round trip: its frame is fetched
alongside.

### Choosing

- **L1 in Restate**: one system, no unfenced writer, no credentials, and
  cold reads about 10× faster. The whole database, compressed (about
  2.5×), is in Restate, a key per 64 KiB, and every collection lists them
  all; an aged database's promotions rewrite it there.
- **L1 in S3**: Restate holds L0 and the manifest, at most about 1 MiB and
  a few hundred KiB, however large the database grows, and a large
  database's flushes make no S3 request between promotions. Each b-tree
  level a cold query reads costs an S3 GET.

## 7. Earlier layouts

Each layout below was built, measured and replaced by the next; their
reports are in git history (`core/state/docs/` at commit `e015181`, and
v20's at `355a282`).

| Layout                     | What it stored                                                                       | Why it went                                                                                                                                           |
| -------------------------- | ------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| A log                      | Every flush appended to a log, checkpointed now and then into page keys              | Each change was written twice, to the log and then in place                                                                                           |
| Copy-on-write blobs        | A root naming a radix map of chunks, each a base and a delta, all in Restate         | The whole database lived in Restate key by key: 33,000 keys for 100 MB of text                                                                        |
| Blobs in S3                | The same blobs in an object store, the root in Restate, zombies kept apart by claims | Every flush was S3 PUTs: $15 per million one-row updates, over $100 for ten rows                                                                      |
| Tail and checkpoints (v18) | A tail in Restate, and checkpoints of copy-on-write blobs, in S3 or Restate          | The chunk map, bases, deltas and rebase rules; a cold open of 1M rows took up to 1,331 gets                                                           |
| Frame packs (v20)          | A tail of plain mutations; checkpoints of frames and per-page patches, in one store  | Each checkpoint chose per frame between a patch and a rebuild, and re-encoded every page; a large database put its packs, all in Restate or all in S3 |
| Two levels (v21, this)     | A tail; L0, pages as the tail wrote them, in Restate; L1, frames, in S3 or Restate   | –                                                                                                                                                     |

Against v18, frame packs halved the bytes a flush puts on large databases
(a tail page costs what changed since the checkpoint) and brought a cold
open to a manifest and a frame, in about 14% less code. Against v20, two
levels make a checkpoint a copy and let L1 live in S3 while the recent
pages stay in Restate; they cost more bytes on pages changed often between
promotions, and on aged databases kept in Restate (§6).

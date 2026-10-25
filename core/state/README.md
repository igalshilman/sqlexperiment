# State

Transactional state over an asynchronous key/value store, for namespaces with
one owner at a time: the state of a Restate object without Restate's journal
(`journalReplay: false`). It keeps named **records** (small whole values),
paged **files** (4 KiB pages, read and written at random) and **logs**
(entries appended, and truncated from the start). The state is read; it
changes only through a **mutation**, an owned value that collects the records
set, pages written and entries appended, and a **flush** makes everything in the mutation durable at
once, in one round trip. It needs only `get`, `put`, `delete` and `sync` from the backend,
and a listing of its keys to collect the garbage.

The state knows nothing of what writes it. [SQL](../sql/README.md) keeps its
database in the file `db` and moves what it committed into a mutation before
the host flushes it; the durable functions runtime adds a program's memory (a
file and a log) and its run's state (a record). Coordinating them is the
host's: see [the layers](../README.md#layers).

## Use

Implement `kv::KvStore` over your store, then:

```rust,ignore
use state::{PageCache, PageCacheConfig, State};

// Once per process: 256 MiB of pages in memory, 8 GiB in a temporary file.
let cache = PageCache::open(PageCacheConfig::new(256 << 20, 8 << 30)).await?;

// Per session, kept warm in the cache under the namespace's stable key.
let state = State::open(store).cached(&cache, "object-42").await?;
let mut mutation = state.mutation();
mutation.set(&state.record("progress"), "step 7".into())?;
mutation.write(&state.file("image")?, 0, Some(page))?;
mutation.flush().await?; // the record and the page, durable together
```

Nothing is durable before `Mutation::flush`, and then everything in that
mutation is, at once: its tail entry, then the head that names it, in one
round trip. A flush consumes the mutation; a mutation dropped instead
discards its writes. The state shows what was flushed (`record.get()`,
`file.read(page)`), never what a mutation holds. The host must guarantee
one owner per namespace. A failed or cancelled flush ends the state
(`is_usable()` returns false): its outcome is unknown, so settle
outstanding requests and open a new one.

Whoever moves its changes into a mutation keeps a receipt
(`mutation.receipt()`), which tells it whether that mutation was flushed or
discarded. With a page cache, a value can be parked for the namespace's next
state (`state.park(key, value, weight)`), which takes it back
(`state.take_parked(key)`) when nothing changed since: the SQL crate keeps its
engine warm this way.

## How durability and recovery work

The backend is a Restate object's state: single-key puts and deletes, queued
in issue order, and a sync that returns once they are stored. A crash keeps a
prefix of the writes since the last sync. A flush touches many pages, so it
can't be written atomically as pages. Instead, a flush appends what it
changed to a tail, and one key, the head, names the tail and the last
checkpoint: the commit point.

A namespace holds:

- **The head** (`v20/head`): the version, the frame size, the checkpoint
  and the one before, and which tail entries are kept.
- **The tail** (`v20/tail/<version>/<part>`): one entry per flush, the
  mutation as it was: the records set, the log entries appended and
  truncations, the files' sizes, and each page written, a zstd frame of its
  own, compressed against the page as the checkpoint holds it, so a small
  change costs about what changed. An entry past 1 MiB is cut into parts.
- **Manifests** (`v20/manifest/<version>-<nonce>/<part>`): one per
  checkpoint, put once and never changed, in Restate wherever the packs
  are: where every file's frames, every record and every log entry are, in
  its pack or an older one. About 1.7 bytes per frame: 3 KiB for a
  database of 100k rows.
- **Packs** (`checkpoint/<version>-<nonce>`): one per checkpoint that
  stores anything outside its manifest, put once and never changed: the
  frames it wrote, and large records and log entries.

**Frames.** A paged file is a row of frames of `frame_pages` pages (4,
16 KiB), each compressed with zstd on its own (against its file's base, if
it has one), so a read fetches one range of a pack. A file holds at most
10 GiB.

**Flush.** A flush puts its tail entry, then the head that names it, and
syncs once: one round trip. Once the head is stored, the transaction is
durable; before, it doesn't exist.

**Checkpoint.** Once the tail holds `checkpoint_entries` (128) entries or
`checkpoint_bytes` (1 MiB) since the last checkpoint (with packs in Restate,
at most the bytes the checkpoint holds, its packs and its manifest, so a
small database's tail stays short), a flush first writes a checkpoint, as does any flush of a
mutation marked `mutation.checkpoint()`. It rewrites each frame the tail
changed, from the old frame and the tail's pages; or, where the tail changed
only a few of a frame's pages (their patches stay within half the frame's
stored bytes), it _patches_ the frame: it puts those pages, each compressed
against the frame's. It puts the records set and log entries appended
since, inline in the manifest if small, and copies the live ranges of packs
it needs less than half of (compaction): one pack, one put, and its
manifest, queued before the head. The flush's head names it,
the one before becomes the previous checkpoint, and the tail that one
covered is deleted. A checkpoint that fails fails nothing: the flush's
report says why (`FlushReport::checkpoint_failed`), the tail grows, and the
next flush tries again.

**Garbage.** A pack older than the head's previous checkpoint that neither
of its checkpoints' manifests names is garbage: no later head names it; so
is a manifest older than the previous checkpoint. A flush that checkpoints
then collects them, and anyone may, beside the owner
(`State::collect_garbage`). Collecting lists Restate's keys once
(`KvStore::keys`, Restate's `getKeys()`), for the manifests and the packs
kept there, and S3's for the packs kept there; it stores its deletes in
Restate with one sync. A store that cannot list keeps its garbage.

**Readers.** A reader (a shared handler beside the exclusive one) reads the
state it opened, a consistent snapshot, while the owner flushes: packs never
change, and the packs and tail a head names stay until the owner's second
checkpoint after it. A reader that outlives that fails with "is gone".

**Recovery.** There's none: every open reads the head, which names only a
tail, manifests and packs that are stored, then the checkpoint's manifest and the tail
past it, together. A crashed flush's tail entry is past the head's version,
and the next flush overwrites it; its pack and manifest, if it checkpointed, are garbage.

**Packs in Restate or S3.** By default, packs are kept in Restate beside the
head (`blobs::KvBlobs`), in parts of 64 KiB (`v20/pack/<key>/<part>`): the
flush's sync stores them, before the head that names them, and nothing is
in S3. The manifests are in Restate either way, so a cold open reads the
head, then the manifest and the tail, from Restate alone. With
`State::open(store).checkpoints(packs)`, they are in an object store (S3,
`blobs::ObjectStoreBlobs`), uploaded before the flush puts its head. A
pack's name is unique, so a former attempt that Restate fenced may still
upload one, but never over another's, and never one a head names: S3 needs
no fencing, nor conditional puts.

**What a crash leaves.**

| Interrupted          | Result                                      |
| -------------------- | ------------------------------------------- |
| A flush              | Either all of the transaction or none of it |
| A checkpoint         | A pack no head names: garbage               |
| A garbage collection | Repeated by the next one                    |

**Why it is simple.** Packs never change, so a cache keeps their ranges by
location and never invalidates them; and nothing a head names is ever
overwritten, so there is nothing to repair.

**What it relies on.**

- **One owner at a time.** Restate runs one exclusive handler per key, and
  fences a former attempt's writes.
- **Writes in issue order.** A crash keeps a prefix of the writes since the
  last sync, never a write without the ones before it.
- **Reliable storage.** Restate (and S3) return what was stored. Decoding
  checks structure and bounds, and every zstd frame carries a checksum.

[The design](docs/DESIGN.md) has the exact rules, the failure table and the
owner cache; [frame packs](docs/FRAMES.md) has the experiment that made this
layout, with measurements against the ones before.

## Docs

- [Architecture](docs/ARCHITECTURE.md): start here: the data model, the
  state as a sequence of mutations, the head, tail, checkpoints, manifests
  and packs and how they change over time, packs in S3, and the numbers
- [Overview](docs/OVERVIEW.md): the state as files and records changed
  through a mutation, who writes it, and a toy durable functions extension on it
- [Design](docs/DESIGN.md): the model, invariants, flush, checkpoint, open,
  failures, cache and costs
- [Storage format](docs/FORMAT.md): exact encodings
- [Frame packs](docs/FRAMES.md): the tail and checkpoints of compressed
  frames, as implemented, with measurements
- History, of layouts since replaced:
  - [Copy-on-write](docs/COW_EXPERIMENT.md): blobs and a root, which
    replaced the log, with the chunk map's fanout
  - [Blobs in S3](docs/S3_EXPERIMENT.md): the root in Restate, the blobs in
    an object store, kept safe from earlier attempts by claims
  - [Tail and checkpoints](docs/TAIL_EXPERIMENT.md): a tail in Restate and
    checkpoints in S3, still of copy-on-write blobs, which frame packs
    simplified
- [SQL](../sql/README.md): SQLite-compatible SQL on the state, with the
  storage tests and benchmarks that run SQL workloads

## Layout

| Path             | What                                                 |
| ---------------- | ---------------------------------------------------- |
| `src/`, `tests/` | The crate and its tests                              |
| [`docs/`](docs)  | Overview, design, storage format, experiment reports |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # with the in-memory store and fault injection
cargo clippy --locked --all-targets --all-features -- -D warnings
```

The `simulation` feature adds an in-memory KV store, fault injection and a
latency simulator, which the tests here and in [`sql`](../sql) use.

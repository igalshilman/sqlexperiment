# State

Transactional state over an asynchronous key/value store, for namespaces with
one owner at a time: the state of a Restate object without Restate's journal
(`journalReplay: false`). It keeps named **records** (small whole values) and
paged **files** (4 KiB pages, read and written at random). The state is read;
it changes only through a **WAL**, an owned value that collects the records set
and pages written, and a **flush** makes everything in the WAL durable at
once, in one round trip. It needs only `get`, `put` and `delete` from the backend.

What writes a WAL is a **participant**: it keeps changes of its own, moves
them into a WAL before the flush, and may leave state idle for the
namespace's next state. [SQL](../sql/README.md) is one, on the file `db`; the
durable functions runtime adds a program's memory (a file) and its run's
state (a record).

## Use

Implement `kv::KvStore` over your store, then:

```rust,ignore
use state::{PageCache, PageCacheConfig, State};

// Once per process: 256 MiB of pages in memory, 8 GiB in a temporary file.
let cache = PageCache::open(PageCacheConfig::new(256 << 20, 8 << 30)).await?;

// Per session, kept warm in the cache under the namespace's stable key.
let state = State::open(store).cached(&cache, "object-42").await?;
let mut wal = state.wal();
wal.set(&state.record("progress"), "step 7".into())?;
wal.write(&state.file("image")?, 0, Some(page))?;
wal.flush().await?; // the record and the page, durable together
```

Nothing is durable before `Wal::flush`, and then everything in that WAL is, at
once: its blobs, then the root that names them, in one round trip. A flush
consumes the WAL; a WAL dropped instead discards its writes. The state shows
what was flushed (`record.get()`, `file.read(page)`), never what a WAL holds.
The host must guarantee one owner per namespace. A failed or cancelled flush
ends the state (`is_usable()` returns false): its outcome is unknown, so
settle outstanding requests and open a new one.

A participant joins under a name (`state.join("sql")`), one at a time per
name. While it says it holds changes not yet in a WAL (`set_unflushed(true)`),
every WAL of the state refuses to flush. Once it has moved them into one, a
receipt (`wal.receipt()`) tells it whether that WAL was flushed or discarded.
With a page cache, it may `park` state for the next state of the namespace,
which takes it back (`take_idle`) when nothing changed since: the SQL crate
keeps its engine warm this way.

## How durability and recovery work

The backend is a Restate object's state: single-key puts and deletes, queued
in issue order, and a sync that returns once they are stored. A crash keeps a
prefix of the writes since the last sync. A flush touches many pages, so it
can't be written atomically as pages. Instead, nothing is ever overwritten
but one key, the root, which names everything else: copy-on-write.

A namespace holds:

- **The root** (`v16/root`): the version, the directory of files (paged
  files, such as the SQL database's, and records), the small records, and,
  for each chunk of 4 pages of each paged file, the blobs that hold it.
- **Blobs** (`v16/blob/<n>`): one zstd frame each, written once and never
  changed: a chunk's _base_ (its pages), maybe a _delta_ (its pages now,
  compressed against the base, so it costs about what changed), or a large
  record.

**Flush.** A flush puts a new delta for each chunk it changed (or a new base,
once the delta grows past a quarter of the base), then the root that names
them, and syncs once: one round trip. Once the root is stored, the
transaction is durable; before, it doesn't exist.

**One rule.** A blob is put before the first root that names it, and deleted
only once no root a reader may still hold names it: 16 flushes after the
last root that named it, by default.

**Recovery.** There's none: every open reads the root, which names only
blobs that are stored. A crashed flush's blobs are numbered past what the
root names, and the next flush overwrites them.

**What a crash leaves.**

| Interrupted             | Result                                      |
| ----------------------- | ------------------------------------------- |
| A flush                 | Either all of the transaction or none of it |
| A deletion of old blobs | Repeated by the next flush                  |

**Why it is simple.** Blobs never change, so a cache keeps them by number and
never invalidates them, and a reader (a shared handler beside the exclusive
one) reads the root it opened, a consistent snapshot, while the owner
flushes. A paged file holds at most 64 MiB.

**What it relies on.**

- **One owner at a time.** Restate runs one exclusive handler per key, and
  fences a former attempt's writes.
- **Writes in issue order.** A crash keeps a prefix of the writes since the
  last sync, never a write without the ones before it.
- **Reliable storage.** Restate returns what was stored. Values are checked
  for structure and bounds, and zstd's frame checksums.

[The design](docs/DESIGN.md) has the exact rules, the failure table and the
owner cache; [the experiment](docs/COW_EXPERIMENT.md) compares it with the log
it replaced.

## Docs

- [Overview](docs/OVERVIEW.md): the state as files and records changed
  through a WAL, who writes it, and a toy durable functions extension on it
- [Design](docs/DESIGN.md): the model, invariants, flush, open, failures,
  cache and costs
- [Storage format](docs/FORMAT.md): exact encodings
- [Copy-on-write](docs/COW_EXPERIMENT.md): the experiment that replaced the
  log, with measurements and what it simplified
- [SQL](../sql/README.md): SQLite-compatible SQL on the state, with the
  storage tests and benchmarks that run SQL workloads

## Layout

| Path             | What                                   |
| ---------------- | -------------------------------------- |
| `src/`, `tests/` | The crate and its tests                |
| [`docs/`](docs)  | Design, storage format, the experiment |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # with the in-memory store and fault injection
cargo clippy --locked --all-targets --all-features -- -D warnings
```

The `simulation` feature adds an in-memory KV store, fault injection and a
latency simulator, which the tests here and in [`sql`](../sql) use.

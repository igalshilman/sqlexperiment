# State

Transactional state over an asynchronous key/value store, for namespaces with
one owner at a time: the state of a Restate object without Restate's journal
(`journalReplay: false`). It keeps named **records** (small whole values) and
paged **files** (4 KiB pages, read and written at random). The state is read;
it changes only through a **WAL**, an owned value that collects the records set
and pages written, and a **flush** makes everything in the WAL durable in one
put, usually. It needs only `get`, `put` and `delete` from the backend.

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

Nothing is durable before `Wal::flush`, and then everything in that WAL is, in
one put. A flush consumes the WAL; a WAL dropped instead discards its writes.
The state shows what was flushed (`record.get()`, `file.read(page)`), never
what a WAL holds. The host must guarantee one owner per namespace. A failed or
cancelled flush ends the state (`is_usable()` returns false): its outcome is
unknown, so settle outstanding requests and open a new one.

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
can't be written atomically as pages. Like SQLite's write-ahead log, this
crate makes a flush durable through a log instead, and copies pages into
place later; here the log lives in one key.

A namespace holds:

- **The commit record** (`commit`): the version, the directory of files (paged
  files, such as the SQL database's, and records, with their sizes), and
  the log since the last checkpoint: each transaction's changed pages, compressed
  against their previous images. The record holds the log's last KiB or so
  inline; older parts, and large transactions, are stored at `log/<n>` keys,
  which the record names.
- **Pages** (`page/<file>/<n>`): each page of a paged file as of the last
  checkpoint, overwritten in place.

A page's current value is its latest image in the record's log, or else its
page key.

**Flush.** A flush is one put: the record, with the log so far plus the WAL's
writes as one transaction. Once it is stored, the transaction is durable;
before, it doesn't exist. A large transaction's segments are put before the
record that names them. Once the inline log outgrows a KiB, a flush puts it,
sealed, into a stored value, which the record names: so every flush rewrites a
small record.

**Checkpoint.** Once the log holds more than a few transactions, a flush also
copies the latest image of each page in it to its page key, after the record,
then puts a record with an empty log. A half-finished checkpoint is harmless,
because the record before it still holds every image it was copying, and
replays over whatever landed. The flush and its checkpoint share one sync:
a flush is one round trip.

**Recovery.** There's no separate recovery step; every open reads the record
and replays its log. Whatever a failed flush left (segments no record names,
pages a checkpoint wrote) is ignored, or covered by the log.

**What a crash leaves.** Interrupting any step leaves a state the next open
reads correctly:

| Interrupted                | Result                                                  |
| -------------------------- | ------------------------------------------------------- |
| A flush                    | Either all of the transaction or none of it             |
| A checkpoint               | Pages hold old or new images; the log replays over them |
| A deletion of old segments | Repeated by a later flush                               |

**What it relies on.**

- **One owner at a time.** Restate runs one exclusive handler per key, and
  fences a former attempt's writes.
- **Writes in issue order.** A crash keeps a prefix of the writes since the
  last sync, never a write without the ones before it.
- **Reliable storage.** Restate returns what was stored. Values are checked
  for structure and bounds, not checksummed.

[The design](docs/DESIGN.md) has the exact invariants, the failure table and
the owner cache.

## Docs

- [Overview](docs/OVERVIEW.md): the state as files and records changed
  through a WAL, who writes it, and a toy durable functions extension on it
- [Design](docs/DESIGN.md): the model, invariants, flush, open, failures,
  cache and costs
- [Storage format](docs/FORMAT.md): exact encodings
- [Review and validation](docs/REVIEW.md): what was tested and how
- [Protocol specification](spec/README.md): the protocol in TLA+, model-checked
  with TLC, and the monitor that checks the code against it
- [SQL](../sql/README.md): SQLite-compatible SQL on the state, with the
  storage tests and benchmarks that run SQL workloads

## Layout

| Path             | What                           |
| ---------------- | ------------------------------ |
| `src/`, `tests/` | The crate and its tests        |
| [`docs/`](docs)  | Design, storage format, review |
| [`spec/`](spec)  | The protocol in TLA+           |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # with the in-memory store and fault injection
cargo clippy --locked --all-targets --all-features -- -D warnings
```

The `simulation` feature adds an in-memory KV store, fault injection and a
latency simulator, which the tests here and in [`sql`](../sql) use.

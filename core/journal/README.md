# Journal

A durable journal over an asynchronous key/value store, for namespaces with
one owner at a time (such as a Restate virtual object): SQLite-compatible
SQL, with named records and paged files committed in the same put. It runs Turso's Rust
engine (`turso_core` 0.8.2) unmodified and needs only `get`, `put` and
`delete` from the backend. Changes become durable per session (`save()`), for
hosts that journal and replay, or per SQL commit as in SQLite, for objects
without a journal. Either way, a save is usually one put.

An experiment, not a production backend: see [the design](docs/DESIGN.md).

## Use

Implement `kv::KvStore` over your store, then:

```rust,ignore
use journal::{Durability, Journal, PageCache, PageCacheConfig, Sql, StorageOptions};

// Once per process: a page cache in memory and on a local disk file.
let cache = PageCache::open(PageCacheConfig::new("/var/cache/pages", 256 << 20, 8 << 30)).await?;

// Per session, with the namespace's stable key.
let options = StorageOptions {
    cache: Some(cache.namespace("object-42")),
    ..StorageOptions::default()
};
let mut db = Sql::open_with_options(store, options).await?;
db.execute("INSERT INTO t VALUES(1)").await?;
let saved = db.save().await?; // durable once this returns Ok
```

Bind values with `query_with`/`execute_with` rather than formatting them into
SQL. SQL changes are private until `save()`. `Ok` means committed. The host must
guarantee one owner per namespace.

An SQL error (syntax, a constraint, a missing table) undoes just that statement,
as in SQLite, and the session goes on. A storage error, a cancelled call or a
failed commit ends the session (`is_usable()` returns false): settle
outstanding requests and open a new one; a failed commit's outcome is unknown.

Or make every SQL commit durable, as in SQLite, with no `save()`:

```rust,ignore
let options = StorageOptions {
    cache: Some(cache.namespace("object-42")),
    durability: Durability::Commit,
    ..StorageOptions::default()
};
let mut db = Sql::open_with_options(store, options).await?;
db.execute("INSERT INTO t VALUES(1)").await?; // durable once this returns Ok
db.transaction(async |db| {                    // durable as a whole, or rolled back
    db.execute("UPDATE stock SET n = n - 1 WHERE id = 7").await?;
    db.execute("INSERT INTO shipments VALUES(7)").await
}).await?;
```

Or keep more than SQL: a `Journal` holds paged files and records beside the
SQL database (its file `db`), and its `commit()` makes all of them durable
in one put. The SQL is then one participant (`Durability::Participant`):
`flush()` moves what it committed into its file, and the host commits.

```rust,ignore
let options = StorageOptions {
    durability: Durability::Participant,
    ..StorageOptions::default()
};
let journal = Journal::open_with_options(store, options).await?;
let mut db = Sql::open(&journal).await?;
db.execute("INSERT INTO t VALUES(1)").await?;
db.flush().await?;
journal.record("progress").set("step 2".into());
journal.file("image")?.write(0, Some(page))?;
journal.commit().await?; // the row, the record and the page, durable together
```

## How durability and recovery work

The backend is a Restate object's state: single-key puts and deletes, queued
in issue order, and a sync that returns once they are stored. A crash keeps a
prefix of the writes since the last sync. A database change touches many
pages, so it can't be written atomically as pages. Like SQLite's WAL, this
crate commits through a log instead, and copies pages into place later; here
the log lives in one key.

A namespace holds:

- **The commit record** (`commit`): the version, the directory of files (the
  SQL database's, the host's paged files and records, with their sizes), and
  the log since the last checkpoint: each transaction's changed pages, compressed
  against their previous images. The record holds the log's last KiB or so
  inline; older parts, and large transactions, are stored at `log/<n>` keys,
  which the record names.
- **Pages** (`page/<file>/<n>`): each page of a paged file as of the last
  checkpoint, overwritten in place.

A page's current value is its latest image in the record's log, or else its
page key.

**Commit.** A transaction is one put: the record, with the log so far plus
this transaction. Once it is stored, the transaction is durable; before, it
doesn't exist. A large transaction's segments are put before the record that
names them. Once the inline log outgrows a KiB, a commit puts it, sealed,
into a stored value, which the record names: so every commit rewrites a
small record.

**Checkpoint.** Once the log holds more than a few transactions, a save also
copies the latest image of each page in it to its page key, after the record,
then puts a record with an empty log. A half-finished checkpoint is harmless,
because the record before it still holds every image it was copying, and
replays over whatever landed. The commit and its checkpoint share one sync:
a save is one round trip.

**Recovery.** There's no separate recovery step; every open reads the record
and replays its log. Whatever a failed save left (segments no record names,
pages a checkpoint wrote) is ignored, or covered by the log.

**What a crash leaves.** Interrupting any step leaves a state the next open
reads correctly:

| Interrupted                | Result                                                  |
| -------------------------- | ------------------------------------------------------- |
| A commit                   | Either all of the transaction or none of it             |
| A checkpoint               | Pages hold old or new images; the log replays over them |
| A deletion of old segments | Repeated by a later save                                |

**What it relies on.**

- **One owner at a time.** Restate runs one exclusive handler per key, and
  fences a former attempt's writes.
- **Writes in issue order.** A crash keeps a prefix of the writes since the
  last sync, never a write without the ones before it.
- **Reliable storage.** The backend returns what was stored. Values are
  checked for structure and bounds, not checksummed.

[The design](docs/DESIGN.md) has the exact invariants, the failure table and
the owner cache.

## Docs

- [Design](docs/DESIGN.md): the model, invariants, save, open, failures, cache
  and costs
- [Storage format](docs/FORMAT.md): exact encodings
- [Review and validation](docs/REVIEW.md): what was tested and how
- [Protocol specification](spec/README.md): the protocol in TLA+, model-checked
  with TLC, and the monitor that checks the code against it
- [Benchmarks](benches/README.md)
- [Node.js addon](../../typescript/packages/sql-node/README.md): the same over a key/value store
  implemented in JavaScript, in `typescript/packages/sql-node/`
- [Python extension](../../python/journal/README.md): the same over a
  key/value store implemented in Python, in `python/journal/`

It is used by a patched Restate TypeScript SDK, for SQL in virtual objects:
see [the repository's README](../../README.md).

## Layout

| Path                          | What                                |
| ----------------------------- | ----------------------------------- |
| `src/`, `tests/`, `examples/` | The crate, its tests and an example |
| [`benches/`](benches)         | Benchmarks and their results        |
| [`docs/`](docs)               | Design, storage format, review      |
| [`spec/`](spec)               | The protocol in TLA+                |

## Build and test

From this directory:

```sh
cargo test --locked
cargo test --locked --features simulation   # storage, cache and failure campaigns
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo run --locked --features simulation --example lookup
(cd ../packages/sql-node && npm run build && npm test)   # the addon
```

The `simulation` feature adds an in-memory KV store, fault injection and a
latency simulator, used by most tests and all benchmarks. The addon needs node
20 or later.

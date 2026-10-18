# SQLite KV

SQLite-compatible SQL over an asynchronous key/value store, for namespaces with
one owner at a time (such as a Restate virtual object). It runs Turso's Rust
engine (`turso_core` 0.8.2) unmodified and needs only `get`, `put` and
`delete` from the backend. Changes become durable per session (`save()`), for
hosts that journal and replay, or per SQL commit as in SQLite, for objects
without a journal. Either way, a save is usually one put.

An experiment, not a production backend: see [the design](docs/DESIGN.md).

## Use

Implement `kv::KvStore` over your store, then:

```rust,ignore
use sqlite_kv::{Durability, PageCache, PageCacheConfig, Session, StorageOptions};

// Once per process: a page cache in memory and on a local disk file.
let cache = PageCache::open(PageCacheConfig::new("/var/cache/pages", 256 << 20, 8 << 30)).await?;

// Per session, with the namespace's stable key.
let options = StorageOptions {
    cache: Some(cache.namespace("object-42")),
    ..StorageOptions::default()
};
let mut db = Session::open_with_options(store, options).await?;
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
let mut db = Session::open_with_options(store, options).await?;
db.execute("INSERT INTO t VALUES(1)").await?; // durable once this returns Ok
db.transaction(async |db| {                    // durable as a whole, or rolled back
    db.execute("UPDATE stock SET n = n - 1 WHERE id = 7").await?;
    db.execute("INSERT INTO shipments VALUES(7)").await
}).await?;
```

## How durability and recovery work

The backend only offers single-key puts, in no particular order: a write is
durable once acknowledged, and a crash may keep any of the others. A database
change touches many pages, so it can't be written atomically as pages. Like
SQLite's WAL, this crate commits through a log instead, and copies pages into
place later; here the log lives in one key.

A namespace holds:

- **The commit record** (`commit`): the database's version and size, and the
  log since the last checkpoint: each transaction's changed pages, compressed
  against their previous images. The record holds the log's last few KiB
  inline; older parts, and large transactions, are stored at `log/<n>` keys,
  which the record names.
- **Pages** (`page/<n>`): each page as of the last checkpoint, overwritten in
  place.

A page's current value is its latest image in the record's log, or else its
page key.

**Commit.** A transaction is one put: the record, with the log so far plus
this transaction. When that put is acknowledged, the transaction is durable;
before it, it doesn't exist. A large transaction's segments are put first,
and the record names them only once they are acknowledged. Once the inline
log outgrows a few KiB, a commit also puts it, sealed, into a stored value,
which the next commit names: so every commit rewrites a small record.

**Checkpoint.** Once the log holds more than a few transactions, a save also
copies the latest image of each page in it to its page key. Only once those
copies are acknowledged does it put a record with an empty log. A
half-finished checkpoint is harmless, because the log still holds every image
it was copying and replays over whatever landed.

**Recovery.** There's no separate recovery step; every open reads the record
and replays its log. Whatever a failed save left (segments no record names,
pages a checkpoint wrote) is ignored, or covered by the log.

**What a crash leaves.** Interrupting any step leaves a state the next open
reads correctly:

| Interrupted                 | Result                                                  |
| --------------------------- | ------------------------------------------------------- |
| A commit                    | Either all of the transaction or none of it             |
| A checkpoint                | Pages hold old or new images; the log replays over them |
| A deletion of old segments  | Repeated by a later save                                |

**What it relies on.**

- **One owner at a time.** The host must guarantee it, and settle every
  outstanding request before another owner opens.
- **Acknowledged means durable.** No order between writes is assumed.
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
- [Python extension](../../python/sqlite-kv/README.md): the same over a
  key/value store implemented in Python, in `python/sqlite-kv/`

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

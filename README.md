# SQLite KV

SQLite-compatible SQL over an asynchronous key/value store, for namespaces with
one owner at a time (such as a Restate virtual object). It runs Turso's Rust
engine (`turso_core` 0.8.2) unmodified and needs only `get`, `put` and
`delete` from the backend. A save is usually one put.

An experiment, not a production backend: see [the design](docs/DESIGN.md).

## Use

Implement `kv::KvStore` over your store, then:

```rust,ignore
use sqlite_kv::{PageCache, PageCacheConfig, Session, StorageOptions};

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

SQL changes are private until `save()`. `Ok` means committed; an error means the
outcome is unknown, so settle outstanding requests and reopen. The host must
guarantee one owner per namespace.

## Docs

- [Design](docs/DESIGN.md): the model, invariants, save, open, failures, cache
  and costs
- [Storage format](docs/FORMAT.md): exact encodings
- [Review and validation](docs/REVIEW.md): what was tested and how
- [Benchmarks](benches/README.md)

## Build and test

```sh
cargo test --locked
cargo test --locked --features simulation   # storage, cache and failure campaigns
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo run --locked --features simulation --example lookup
```

The `simulation` feature adds an in-memory KV store, fault injection and a
latency simulator, used by most tests and all benchmarks.

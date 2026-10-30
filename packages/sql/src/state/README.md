# State

Transactional state over an asynchronous key/value store, for namespaces with
one owner at a time: the state of a Restate object without Restate's journal
(`journalReplay: false`). It keeps named **records** (small whole values),
and paged **files** (4 KiB pages, read and written at random). The state is
read; it changes only through a **mutation**, an owned value that collects the
records set and pages written, and a **flush** makes everything in the mutation durable at
once, in one round trip. It needs only `get`, `put`, `delete` and `sync` from the backend,
and a listing of its keys to collect the garbage.

The state knows nothing of what writes it. [SQL](../sql/README.md) keeps its
database in the file `db` and moves what it committed into a mutation before
the host flushes it, and the host adds records of its own beside it.
Coordinating them is the host's: see [the layers](../../README.md#inside-the-layers).

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
(`state.take_parked(key)`) when nothing changed since: SQL keeps its
engine warm this way.

## How it works

A flush puts its mutation as one entry of a **tail** in Restate, then the
**head**, the one key whose write commits, and syncs once. Now and then a
flush first writes a **checkpoint**: a manifest in Restate that indexes the
whole state, and a pack of what changed since the checkpoint before, in
Restate, until the packs there hold a few MiB, then promoted to S3 if the
state has one. Nothing a head names is ever overwritten, so a
crash leaves nothing to repair. It relies on one owner at a time, which
Restate's virtual objects give, and on a crash keeping a prefix of the
writes since the last sync.

## Docs

- [Architecture](docs/ARCHITECTURE.md): start here. The data model, the
  head, tail, checkpoints, manifests and packs and how they change over
  time, L0 and L1, the numbers, and the layouts this one replaced
- [Design](docs/DESIGN.md): the rules the architecture leaves out: retries,
  readers, the SQL engine, the owner cache, costs and limits
- [Storage format](docs/FORMAT.md): exact encodings
- [SQL](../sql/README.md): SQLite-compatible SQL on the state, with the
  storage tests and benchmarks that run SQL workloads

## Layout

| Path                                       | What                                                      |
| ------------------------------------------ | --------------------------------------------------------- |
| `mod.rs` and the files beside it, `tests/` | The module and its tests                                  |
| `simulation/`                              | In-memory stores, fault injection and a latency simulator |
| [`docs/`](docs)                            | Architecture, design, storage format                      |

## Build and test

From `packages/sql`:

```sh
cargo test --locked --release state::     # the state's tests, with the in-memory store and fault injection
cargo clippy --locked --all-targets -- -D warnings
```

The `simulation` module, an in-memory KV store, fault injection and a
latency simulator, is always on in tests; the tests here and in
[`sql`](../sql) use it.

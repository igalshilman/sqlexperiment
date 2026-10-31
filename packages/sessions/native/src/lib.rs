//! The Node.js addon of restate-sessions: an object's SQLite-compatible
//! database, kept in its Restate state, over a key/value store the host
//! implements in JavaScript.
//!
//! One entry point, [`Database`]: [`Database::open`] opens a session of an
//! object's database over its [`KvStore`], and every SQL commit is durable
//! when its call returns. Inside, three private modules, each on the one
//! before:
//!
//! - `state`: the database's file and its records over the key/value store,
//!   changed through a mutation that a flush makes durable at once
//!   (`src/state/README.md`).
//! - `sql`: the [`Database`], SQL on the state's file, each commit flushed
//!   with the host's records (`src/sql/README.md`).
//! - `node`: the napi bindings, around [`Database`].
//!
//! With the feature `bench`, `simulation` is public too: the in-memory
//! stores, fault injection and latency simulator the benchmarks run on.

#[cfg(not(any(test, feature = "bench")))]
mod node;
mod sql;
mod state;

pub use sql::{Database, Numeric, Open, Refused, Rows, Value};
pub use state::{PageCache, PageCacheConfig, Result, StateOptions, blobs::BlobStore, kv::KvStore};

#[cfg(feature = "bench")]
pub use sql::IoStats;
#[cfg(feature = "object-store")]
pub use state::blobs::ObjectStoreBlobs;
#[cfg(any(test, feature = "bench"))]
pub use state::simulation;

/// Return to the executor once, even when every awaited future is ready.
async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

//! Backend contract: whole-value reads and atomic, durable single-key mutations.
//!
//! The adapter issues concurrent gets, and concurrent puts or deletes of distinct
//! keys within one save. It never has two requests of one attempt outstanding
//! against the same key. There is no multi-key atomicity, and no order between
//! writes: the protocol relies on a write only once it is acknowledged.
//!
//! Success acknowledges durability of that key. No partial value may ever
//! become visible, and every value a get returns is durable. An error or
//! cancellation has an unknown outcome: the mutation may be absent or durable.
//! Dropping a future does not cancel a remote request; the host must settle it
//! before recovery or before handing the namespace to another owner.
//!
//! Restate state, with no journal, is such a store when its host acknowledges
//! a write once the server stored it.
use bytes::Bytes;
use futures::future::BoxFuture;
use std::io;

/// A single-namespace key/value store; see the module documentation.
pub trait KvStore: Send + Sync {
    /// Return the latest durable complete value, or `None` for an absent key.
    /// `Bytes` lets a network client hand over its receive buffer without copying.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Atomically replace a whole value; `Ok(())` means durable.
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>>;
    /// Durably remove a key. Deleting a missing key must succeed.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
}

/// A store that queues writes and stores them, in order, at a `sync`: what
/// the Restate SDKs offer a handler. [`Synced`] makes it a [`KvStore`].
pub trait Queued: Send + Sync {
    /// The latest value: stored, or written by this attempt.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Queue a write: a put, or with `None` a delete.
    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()>;
    /// Wait until every write queued so far is stored.
    fn sync(&self) -> BoxFuture<'_, io::Result<()>>;
}

/// A [`Queued`] store as a [`KvStore`]: a write is acknowledged once a `sync`
/// issued after it has completed. Writes queued together share one `sync`.
pub struct Synced<Q> {
    queue: Q,
    /// Writes queued so far.
    queued: std::sync::atomic::AtomicU64,
    /// Writes a completed `sync` stored, behind the lock a `sync` holds.
    stored: futures::lock::Mutex<u64>,
}

impl<Q: Queued> Synced<Q> {
    pub fn new(queue: Q) -> Self {
        Self {
            queue,
            queued: Default::default(),
            stored: futures::lock::Mutex::new(0),
        }
    }

    pub fn queue(&self) -> &Q {
        &self.queue
    }

    /// Queue a write now, then wait until a `sync` stored it: the writes of
    /// one save are queued before the first `sync` starts, and share it.
    fn write<'a>(&'a self, key: &str, value: Option<Bytes>) -> BoxFuture<'a, io::Result<()>> {
        use std::sync::atomic::Ordering::SeqCst;
        if let Err(error) = self.queue.queue(key, value) {
            return Box::pin(std::future::ready(Err(error)));
        }
        let ticket = self.queued.fetch_add(1, SeqCst) + 1;
        Box::pin(async move {
            let mut stored = self.stored.lock().await;
            if *stored < ticket {
                // Covers every write queued until now, not only this one.
                let target = self.queued.load(SeqCst);
                self.queue.sync().await?;
                *stored = target;
            }
            Ok(())
        })
    }
}

impl<Q: Queued> KvStore for Synced<Q> {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        self.queue.get(key)
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        self.write(key, Some(value))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.write(key, None)
    }
}

#[cfg(test)]
#[path = "../tests/unit/kv.rs"]
mod tests;

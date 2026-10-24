//! Backend contract: a Restate object's state, as a handler without a journal
//! sees it.
//!
//! - **Writes are queued, in order.** [`KvStore::put`] and [`KvStore::delete`]
//!   return at once. The store keeps them in issue order: a crash loses a
//!   suffix of the writes not yet synced, never a write without the ones
//!   before it.
//! - **A sync makes them durable.** [`KvStore::sync`] completes once every
//!   write issued before it is stored. Its failure, or a crash, leaves an
//!   unknown prefix of them stored.
//! - **Reads see the writes.** [`KvStore::get`] returns the latest value,
//!   stored or written by this owner since.
//!
//! So a write needs no acknowledgement for a later one to rely on it: if the
//! later one survives, so does it. A value is atomic: no partial value is ever
//! visible.
//!
//! A store may keep blobs where an earlier attempt of the owner, still
//! running, can write too: S3 beside Restate, say ([`crate::blobs`]), where
//! Restate fences the root but nothing fences the blobs. It says so
//! ([`KvStore::write_once_blobs`]), and the state then never writes a blob
//! key twice.
use bytes::Bytes;
use futures::future::BoxFuture;
use std::io;

/// A single-namespace key/value store; see the module documentation.
pub trait KvStore: Send + Sync {
    /// The latest value of `key`: stored, or written since; `None` if absent.
    /// `Bytes` lets a network client hand over its receive buffer without copying.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Queue the replacement of a whole value, after every write issued before.
    fn put(&self, key: &str, value: Bytes) -> io::Result<()>;
    /// Queue the removal of a key, after every write issued before. Deleting
    /// a missing key succeeds.
    fn delete(&self, key: &str) -> io::Result<()>;
    /// Wait until every write issued so far is durable.
    fn sync(&self) -> BoxFuture<'_, io::Result<()>>;
    /// Whether the store refuses to write a blob key that holds a value. The
    /// state then numbers the blobs of each writer's first flush from a
    /// random jump past the root's next blob, so that another writer of the
    /// same root (an attempt fenced from the root, or one whose flush failed)
    /// is unlikely to have used the numbers; if one did, the store refuses,
    /// the flush fails, and the next open jumps again. A wrapper forwards it.
    fn write_once_blobs(&self) -> bool {
        false
    }
}

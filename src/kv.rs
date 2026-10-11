//! Backend contract: whole-value reads and atomic, durable single-key mutations.
//!
//! The adapter issues concurrent gets, and concurrent puts or deletes of distinct
//! keys within one save. It never has two requests of one attempt outstanding
//! against the same key. There is no multi-key atomicity.
//!
//! Success acknowledges durability of that key only. No partial value may ever
//! become visible. An error or cancellation has an unknown outcome: the mutation may
//! be absent, visible but not durable, or durable. Dropping a future does not cancel
//! a remote request; the host must settle it before recovery or before handing the
//! namespace to another owner.
use bytes::Bytes;
use futures::future::BoxFuture;
use std::io;

/// A single-namespace key/value store; see the module documentation.
pub trait KvStore: Send + Sync {
    /// Return the latest visible complete value, or `None` for an absent key.
    /// `Bytes` lets a network client hand over its receive buffer without copying.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Atomically replace a whole value; `Ok(())` means durable and visible.
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>>;
    /// Durably remove a key. Deleting a missing key must succeed.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
    /// Whether every value a get returns is already durable, even one written by a
    /// failed put. Most replicated stores only expose replicated writes. When false
    /// (the conservative default), open re-puts the log tail it did not write
    /// itself before exposing it.
    fn durable_when_visible(&self) -> bool {
        false
    }
}

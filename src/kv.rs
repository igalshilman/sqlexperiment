//! Backend contract: whole-value reads and atomic, durable single-key mutations.
//!
//! The adapter serializes puts and deletes. Implementations may assume at most one
//! mutation is active for the owned namespace, but must support concurrent `get`
//! futures. A backend shared across independently owned namespaces is responsible
//! for any stronger global serialization it requires.
//!
//! Returning success acknowledges durability of that key, not a flush of unrelated
//! failed operations. No partial/torn value may become visible. Dropping a future
//! is not evidence that the remote request was cancelled; the host must settle it
//! before recovery. These are requirements on the backend, not guarantees the
//! adapter can manufacture from a weaker storage API.
use futures::future::BoxFuture;
use std::io;

/// Each successful put/delete is atomic, durable and visible to later reads,
/// including after recovery. Deleting a missing key succeeds durably.
/// Reads return the latest visible value; there is no multi-key atomicity.
/// Errors/cancellation have unknown outcomes: a mutation may be absent, visible
/// but not durable, or durable. No partial value may ever appear. The host must keep
/// exclusive ownership until even cancelled operations have settled; no late
/// head write may arrive after ownership is handed to another session.
pub trait KvStore: Send + Sync {
    /// Return the latest visible complete value, or `None` for an absent key.
    /// Multiple reads may be polled concurrently, including head and intent on open.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>>;
    /// Atomically replace a whole value; `Ok(())` means durable and visible.
    /// An error/cancellation may still have changed the key. No subsequent mutation
    /// is submitted by this save after an error; reopening resolves the outcome.
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>>;
    /// Durably remove a key; deleting an already missing key must also succeed.
    /// Recovery relies on this idempotence when retrying partly completed cleanup.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
}

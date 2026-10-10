//! Asynchronous, atomic and durable single-key mutations.
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
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>>;
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
}

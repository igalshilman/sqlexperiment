//! Critical-path latency in round trips, measured on a virtual clock.
//!
//! Every request takes exactly one round trip. A request issued at virtual time
//! `t` completes at `t + 1`, and the clock advances to the latest completion
//! observed. Requests issued together overlap: a batch of reads costs one round
//! trip, while dependent requests cost one each. Multiply by a backend's RTT to
//! estimate wall time; CPU time, bandwidth and queueing are not modeled.
use crate::kv::KvStore;
use futures::future::BoxFuture;
use std::{io, sync::Mutex};

pub struct LatencyKv<S> {
    pub inner: S,
    now: Mutex<u64>,
    /// Override of the inner store's [`KvStore::durable_when_visible`].
    strong: Option<bool>,
}

impl<S: KvStore> LatencyKv<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            now: Mutex::new(0),
            strong: None,
        }
    }

    /// Model a backend whose visible values are always durable (or not). Only for
    /// measurements: the inner simulator still exposes unacknowledged writes.
    pub fn with_durable_when_visible(mut self, strong: bool) -> Self {
        self.strong = Some(strong);
        self
    }

    /// Round trips on the critical path since creation or the last reset.
    pub fn round_trips(&self) -> u64 {
        *self.now.lock().unwrap()
    }

    pub fn reset(&self) {
        *self.now.lock().unwrap() = 0;
    }

    fn timed<'a, T: Send + 'a>(
        &'a self,
        request: BoxFuture<'a, io::Result<T>>,
    ) -> BoxFuture<'a, io::Result<T>> {
        let start = self.round_trips();
        Box::pin(async move {
            let result = request.await;
            let mut now = self.now.lock().unwrap();
            *now = (*now).max(start + 1);
            result
        })
    }
}

impl<S: KvStore> KvStore for LatencyKv<S> {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        self.timed(self.inner.get(key))
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        self.timed(self.inner.put(key, value))
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.timed(self.inner.delete(key))
    }
    fn durable_when_visible(&self) -> bool {
        self.strong
            .unwrap_or_else(|| self.inner.durable_when_visible())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/latency.rs"]
mod tests;

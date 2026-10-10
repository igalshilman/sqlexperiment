//! In-memory visibility/durability model and exact logical traffic counters.
use crate::{
    format::{HEAD, Kind, PENDING},
    kv::KvStore,
    yield_now,
};
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

/// Simulator traffic for one object class. Bytes are exact logical key/value
/// lengths; deletes carry only the key. `bytes_deleted` is reclaimed value size,
/// not request traffic. This does not model wire framing, tombstones or replicas.
#[derive(Clone, Debug, Default)]
pub struct Traffic {
    pub gets: u64,
    pub misses: u64,
    pub puts: u64,
    pub deletes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub bytes_deleted: u64,
    pub get_key_bytes: u64,
    pub put_key_bytes: u64,
    pub delete_key_bytes: u64,
}

/// Object class of a key, used to split traffic counters.
pub fn object_kind(key: &str) -> &'static str {
    if key.starts_with(Kind::Block.prefix()) {
        "block"
    } else if key.starts_with(Kind::Node.prefix()) {
        "node"
    } else if key.starts_with(Kind::Log.prefix()) {
        "log"
    } else if key == HEAD {
        "head"
    } else if key == PENDING {
        "intent"
    } else {
        "other"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub read_keys: BTreeSet<String>,
    pub write_keys: BTreeSet<String>,
    pub delete_keys: BTreeSet<String>,
    pub by_kind: BTreeMap<&'static str, Traffic>,
}

impl Stats {
    /// Traffic of one [`object_kind`] class, zero when absent.
    pub fn kind(&self, kind: &str) -> Traffic {
        self.by_kind.get(kind).cloned().unwrap_or_default()
    }
}

#[derive(Default)]
struct Inner {
    values: BTreeMap<String, Vec<u8>>,
    durable: BTreeMap<String, Vec<u8>>,
    stats: Stats,
}

/// Memory simulator with separate visible/durable maps. Successful mutations
/// update both maps for that key. Only explicit failure-simulation hooks can
/// create visible-only changes. Both maps are process memory, not disk storage.
#[derive(Clone, Default)]
pub struct MemoryKv {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryKv {
    /// Independent byte-for-byte copy for deterministic crash simulations.
    pub fn fork(&self) -> Self {
        let inner = self.inner.lock().unwrap();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                values: inner.values.clone(),
                durable: inner.durable.clone(),
                stats: Stats::default(),
            })),
        }
    }
    /// Simulate a crash, optionally allowing specified unacknowledged keys to reach
    /// durable storage first. Each selected mutation persists in its entirety.
    pub fn crash(&self, persist_unacknowledged: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        for key in persist_unacknowledged {
            match inner.values.get(key).cloned() {
                Some(value) => {
                    inner.durable.insert(key.clone(), value);
                }
                None => {
                    inner.durable.remove(key);
                }
            }
        }
        inner.values = inner.durable.clone();
    }
    /// Fault-injection hook, NOT a successful KvStore operation. Stage an atomic
    /// visible-only put, then have the test backend return an error or cancel.
    pub fn simulate_unacknowledged_put(&self, key: &str, value: Vec<u8>) {
        self.inner
            .lock()
            .unwrap()
            .values
            .insert(key.to_owned(), value);
    }
    /// Fault-injection hook, NOT a successful KvStore operation. Stage an atomic
    /// visible-only delete, then have the test backend return an error or cancel.
    pub fn simulate_unacknowledged_delete(&self, key: &str) {
        self.inner.lock().unwrap().values.remove(key);
    }
    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats.clone()
    }
    pub fn reset_stats(&self) {
        self.inner.lock().unwrap().stats = Stats::default();
    }
    /// Diagnostic view of the simulator; not part of the raw K/V interface.
    pub fn keys(&self) -> BTreeSet<String> {
        self.inner.lock().unwrap().values.keys().cloned().collect()
    }
    pub fn stored_bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values
            .values()
            .map(Vec::len)
            .sum()
    }
    pub fn stored_key_bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values
            .keys()
            .map(String::len)
            .sum()
    }
    pub fn block_count(&self) -> usize {
        let prefix = Kind::Block.prefix();
        self.inner
            .lock()
            .unwrap()
            .values
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count()
    }
}

impl KvStore for MemoryKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            yield_now().await;
            let mut inner = self.inner.lock().unwrap();
            let value = inner.values.get(key).cloned();
            inner.stats.gets += 1;
            inner.stats.bytes_read += value.as_ref().map_or(0, |v| v.len() as u64);
            inner.stats.read_keys.insert(key.to_owned());
            let traffic = inner.stats.by_kind.entry(object_kind(key)).or_default();
            traffic.gets += 1;
            traffic.misses += u64::from(value.is_none());
            traffic.bytes_read += value.as_ref().map_or(0, |v| v.len() as u64);
            traffic.get_key_bytes += key.len() as u64;
            Ok(value)
        })
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            yield_now().await;
            let mut inner = self.inner.lock().unwrap();
            let bytes = value.len() as u64;
            inner.stats.puts += 1;
            inner.stats.bytes_written += bytes;
            inner.stats.write_keys.insert(key.to_owned());
            let traffic = inner.stats.by_kind.entry(object_kind(key)).or_default();
            traffic.puts += 1;
            traffic.bytes_written += bytes;
            traffic.put_key_bytes += key.len() as u64;
            inner.durable.insert(key.to_owned(), value.clone());
            inner.values.insert(key.to_owned(), value);
            Ok(())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            yield_now().await;
            let mut inner = self.inner.lock().unwrap();
            inner.stats.deletes += 1;
            inner.durable.remove(key);
            let removed = inner.values.remove(key);
            inner.stats.delete_keys.insert(key.to_owned());
            let traffic = inner.stats.by_kind.entry(object_kind(key)).or_default();
            traffic.deletes += 1;
            traffic.bytes_deleted += removed.map_or(0, |v| v.len() as u64);
            traffic.delete_key_bytes += key.len() as u64;
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/memory.rs"]
mod tests;

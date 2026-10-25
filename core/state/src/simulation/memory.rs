//! In-memory model of a Restate object's state, as [`crate::kv`] describes
//! it, with exact logical traffic counters.
use crate::{
    kv::KvStore,
    simulation::yield_now,
    store::{BLOB_PREFIX, CLAIM_KEY, ROOT_KEY, Root, SWEEP_KEY},
};
use bytes::Bytes;
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

/// Class of a key, used to split traffic counters: `root`, `blob`, `claim`,
/// `sweep` or `other`.
pub fn object_kind(key: &str) -> &'static str {
    if key == ROOT_KEY {
        "root"
    } else if key == CLAIM_KEY {
        "claim"
    } else if key == SWEEP_KEY {
        "sweep"
    } else if key.starts_with(BLOB_PREFIX) {
        "blob"
    } else {
        "other"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub gets: u64,
    pub puts: u64,
    pub deletes: u64,
    pub syncs: u64,
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
    /// Every write issued, applied: what reads see.
    values: BTreeMap<String, Bytes>,
    /// What a crash keeps: the writes synced.
    durable: BTreeMap<String, Bytes>,
    /// The writes issued since the last sync, in order.
    unsynced: Vec<(String, Option<Bytes>)>,
    stats: Stats,
    /// Blob keys ever put, when its blobs are unfenced ([`MemoryKv::unfenced`]).
    put_once: Option<BTreeSet<String>>,
}

/// Memory simulator of a store that keeps writes in issue order: reads see
/// every write issued, a sync makes them durable, and a crash keeps a prefix
/// of the writes since the last sync. Process memory, not disk storage.
#[derive(Clone, Default)]
pub struct MemoryKv {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryKv {
    /// A store whose blobs are not fenced ([`KvStore::unfenced_blobs`]),
    /// like blobs in S3 beside Restate, so the state claims its numbers: a
    /// put of a blob key ever put before panics, as two attempts would.
    pub fn unfenced() -> Self {
        let store = Self::default();
        store.inner.lock().unwrap().put_once = Some(BTreeSet::new());
        store
    }

    /// Replace the value durably at `key`, as a fault of the disk would:
    /// no request, so no check of a blob put twice.
    pub fn corrupt(&self, key: &str, value: Bytes) {
        let mut inner = self.inner.lock().unwrap();
        inner.values.insert(key.to_owned(), value.clone());
        inner.durable.insert(key.to_owned(), value);
    }

    /// Independent byte-for-byte copy for deterministic crash simulations.
    pub fn fork(&self) -> Self {
        let inner = self.inner.lock().unwrap();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                values: inner.values.clone(),
                durable: inner.durable.clone(),
                unsynced: inner.unsynced.clone(),
                stats: Stats::default(),
                put_once: inner.put_once.clone(),
            })),
        }
    }
    /// Simulate a crash that loses every write since the last sync.
    pub fn crash(&self) {
        self.crash_keeping(0);
    }
    /// Simulate a crash that keeps the first `kept` writes since the last
    /// sync, and loses the others.
    pub fn crash_keeping(&self, kept: usize) {
        let mut inner = self.inner.lock().unwrap();
        let unsynced = std::mem::take(&mut inner.unsynced);
        assert!(
            kept <= unsynced.len(),
            "only {} writes since the sync",
            unsynced.len()
        );
        for (key, value) in unsynced.into_iter().take(kept) {
            inner.apply_durable(key, value);
        }
        inner.values = inner.durable.clone();
    }
    /// The writes issued since the last sync, in order: what a crash may lose.
    pub fn unsynced(&self) -> Vec<(String, Option<Bytes>)> {
        self.inner.lock().unwrap().unsynced.clone()
    }
    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats.clone()
    }
    pub fn reset_stats(&self) {
        self.inner.lock().unwrap().stats = Stats::default();
    }
    /// Every key and its value, as reads see them: for seeding a
    /// [`super::MonitorKv`].
    pub fn values(&self) -> Vec<(String, Bytes)> {
        let inner = self.inner.lock().unwrap();
        (inner.values.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    /// Every key and its value, with the root's random database identity
    /// zeroed: two stores that hold the same state compare equal.
    pub fn contents(&self) -> BTreeMap<String, Bytes> {
        let mut contents: BTreeMap<String, Bytes> = self.values().into_iter().collect();
        if let Some(root) = contents.get_mut(ROOT_KEY) {
            let mut decoded = Root::decode(root).expect("a stored root decodes");
            decoded.id = 0;
            *root = decoded.encode();
        }
        contents
    }
    pub fn keys(&self) -> BTreeSet<String> {
        self.inner.lock().unwrap().values.keys().cloned().collect()
    }
    pub fn stored_bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values
            .values()
            .map(Bytes::len)
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
    /// Number of stored blobs.
    pub fn blob_count(&self) -> usize {
        let prefix = BLOB_PREFIX;
        self.inner
            .lock()
            .unwrap()
            .values
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count()
    }
}

impl Inner {
    fn apply_durable(&mut self, key: String, value: Option<Bytes>) {
        match value {
            Some(value) => self.durable.insert(key, value),
            None => self.durable.remove(&key),
        };
    }
}

impl KvStore for MemoryKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            yield_now().await;
            let mut inner = self.inner.lock().unwrap();
            // A shared buffer, like a network client's receive buffer: no copy.
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
    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(put) = &mut inner.put_once
            && key.starts_with(BLOB_PREFIX)
            && !put.insert(key.to_owned())
        {
            panic!("{key} put twice where blobs are unfenced");
        }
        let bytes = value.len() as u64;
        inner.stats.puts += 1;
        inner.stats.bytes_written += bytes;
        inner.stats.write_keys.insert(key.to_owned());
        let traffic = inner.stats.by_kind.entry(object_kind(key)).or_default();
        traffic.puts += 1;
        traffic.bytes_written += bytes;
        traffic.put_key_bytes += key.len() as u64;
        inner.values.insert(key.to_owned(), value.clone());
        inner.unsynced.push((key.to_owned(), Some(value)));
        Ok(())
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.stats.deletes += 1;
        let removed = inner.values.remove(key);
        inner.stats.delete_keys.insert(key.to_owned());
        let traffic = inner.stats.by_kind.entry(object_kind(key)).or_default();
        traffic.deletes += 1;
        traffic.bytes_deleted += removed.map_or(0, |v| v.len() as u64);
        traffic.delete_key_bytes += key.len() as u64;
        inner.unsynced.push((key.to_owned(), None));
        Ok(())
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            yield_now().await;
            let mut inner = self.inner.lock().unwrap();
            inner.stats.syncs += 1;
            for (key, value) in std::mem::take(&mut inner.unsynced) {
                inner.apply_durable(key, value);
            }
            Ok(())
        })
    }
    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        Box::pin(async move {
            yield_now().await;
            Ok(self.inner.lock().unwrap().values.keys().cloned().collect())
        })
    }
    fn unfenced_blobs(&self) -> bool {
        self.inner.lock().unwrap().put_once.is_some()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/memory.rs"]
mod tests;

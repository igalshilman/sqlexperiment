//! Optional least-recently-used cache of encoded immutable objects, shared by
//! successive sessions of one namespace.
//!
//! Committed objects never change: published IDs are not reused, so an entry can
//! never become stale and needs no invalidation. Only objects known to be
//! committed are inserted: values read through a recovered head, and the objects
//! of a save after its head put was acknowledged. Objects of an unacknowledged
//! attempt are never cached, because recovery may delete them and a later attempt
//! may reuse their IDs.
//!
//! Entries are keyed by `(kind, ID)` only, so one cache must serve exactly one
//! namespace. Sharing it between databases returns wrong pages.
use crate::format::{Id, Kind};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

/// A byte-bounded object cache; clones share the same entries.
#[derive(Clone)]
pub struct ObjectCache(Arc<Mutex<Lru>>);

type Object = (Kind, Id);

struct Lru {
    capacity: usize,
    bytes: usize,
    clock: u64,
    /// Value and last-use time.
    entries: HashMap<Object, (Arc<[u8]>, u64)>,
    /// Last-use time to entry, oldest first.
    order: BTreeMap<u64, Object>,
}

impl ObjectCache {
    /// Create a cache holding at most `capacity` bytes of encoded values.
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(Lru {
            capacity,
            bytes: 0,
            clock: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        })))
    }

    /// Total bytes of the cached values.
    pub fn bytes(&self) -> usize {
        self.0.lock().unwrap().bytes
    }

    pub(crate) fn get(&self, object: Object) -> Option<Arc<[u8]>> {
        let mut lru = self.0.lock().unwrap();
        lru.clock += 1;
        let now = lru.clock;
        let (bytes, used) = lru.entries.get_mut(&object)?;
        let (bytes, previous) = (bytes.clone(), std::mem::replace(used, now));
        lru.order.remove(&previous);
        lru.order.insert(now, object);
        Some(bytes)
    }

    pub(crate) fn insert(&self, object: Object, bytes: Arc<[u8]>) {
        let mut lru = self.0.lock().unwrap();
        if bytes.len() > lru.capacity {
            return;
        }
        lru.remove(object);
        lru.clock += 1;
        let now = lru.clock;
        lru.bytes += bytes.len();
        lru.entries.insert(object, (bytes, now));
        lru.order.insert(now, object);
        while lru.bytes > lru.capacity {
            let (_, oldest) = lru.order.pop_first().expect("nonempty when over capacity");
            lru.remove(oldest);
        }
    }

    pub(crate) fn remove(&self, object: Object) {
        self.0.lock().unwrap().remove(object);
    }
}

impl Lru {
    fn remove(&mut self, object: Object) {
        if let Some((bytes, used)) = self.entries.remove(&object) {
            self.bytes -= bytes.len();
            self.order.remove(&used);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/cache.rs"]
mod tests;

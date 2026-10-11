//! Optional owner-process state shared by successive sessions of one namespace:
//! a byte-bounded LRU of checkpointed page values plus what this owner knows to
//! be durable.
//!
//! Pages are overwritten in place, so cached values can go stale. The cache keeps
//! a *base*: the log start of the last durable `meta` this owner saw. A cached
//! page is current unless a committed transaction at or after the base changed
//! it, and while such a transaction is still in the replayed log, its image
//! overrides the cache. Only a checkpoint that moves `meta` past it makes the
//! cached value visible, and only another writer does that without writing
//! through this cache.
//!
//! The cache also remembers the exact `meta` bytes it last read or wrote. If open
//! reads anything else, another writer checkpointed (or this owner's last save
//! failed). Open then reads the segments from the base to the new log start and
//! refreshes the cached pages they changed. Those segments usually still exist:
//! garbage collection deletes them only in the save after the one that
//! advanced `meta`. If any is gone, or there are more than one probe batch, the
//! cache drops all pages instead.
//!
//! The owner also remembers facts that would otherwise cost round trips:
//! - the commit it last had acknowledged, so open need not re-put that tail;
//! - a checkpoint claim: pages through some log position were acknowledged, so the
//!   next save can advance `meta` in the same round trip as its commit, and
//!   replay can start there.
//!
//! One cache must serve exactly one namespace; sharing it returns wrong pages.
use crate::{
    WarmEngine,
    log::{Images, Meta},
    page_codec::{self, Page},
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

/// Owner-process state; clones share it.
#[derive(Clone)]
pub struct PageCache(Arc<Mutex<State>>);

struct State {
    /// Whether entries are decoded pages rather than stored encodings.
    decoded: bool,
    capacity: usize,
    bytes: usize,
    clock: u64,
    /// Value and last-use time by page number.
    entries: HashMap<u64, (Page, u64)>,
    /// Last-use time to page, oldest first.
    order: BTreeMap<u64, u64>,
    /// `meta` bytes as last read or written by this owner; `None` when absent.
    meta: Option<Page>,
    /// Cached pages are current unless changed by a committed transaction at or
    /// after this log position; `None` when unknown.
    base: Option<u64>,
    /// A durable checkpoint this owner may record in `meta`.
    claim: Option<Meta>,
    /// One past this owner's last acknowledged commit.
    committed: Option<u64>,
    /// An engine whose state equals the committed state at `committed`.
    engine: Option<WarmEngine>,
}

impl PageCache {
    /// Create a cache holding at most `capacity` bytes of pages as stored
    /// (usually LZ4-compressed), decoding them on every hit.
    pub fn new(capacity: usize) -> Self {
        Self::with_mode(capacity, false)
    }

    /// Create a cache holding at most `capacity` bytes of decoded 4096-byte
    /// pages: hits need no decoding, but fewer pages fit.
    pub fn decoded(capacity: usize) -> Self {
        Self::with_mode(capacity, true)
    }

    fn with_mode(capacity: usize, decoded: bool) -> Self {
        Self(Arc::new(Mutex::new(State {
            decoded,
            capacity,
            bytes: 0,
            clock: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            meta: None,
            base: Some(Meta::default().log_start),
            claim: None,
            committed: None,
            engine: None,
        })))
    }

    /// Total bytes of the cached page values.
    pub fn bytes(&self) -> usize {
        self.0.lock().unwrap().bytes
    }

    pub(crate) fn is_decoded(&self) -> bool {
        self.0.lock().unwrap().decoded
    }

    /// Where this owner expects the log to start, for a speculative probe.
    pub(crate) fn expected_start(&self) -> Option<u64> {
        let state = self.0.lock().unwrap();
        state.claim.map(|claim| claim.log_start).or(state.base)
    }

    /// Compare the durable `meta` that open read, with log start `log_start`, to
    /// what this owner last saw.
    pub(crate) fn validate(&self, meta: Option<&[u8]>, log_start: u64) -> Reuse {
        let mut state = self.0.lock().unwrap();
        if state.meta.as_deref() == meta {
            return Reuse::Known(state.claim, state.committed);
        }
        state.forget();
        match state.base {
            Some(base) if base <= log_start => Reuse::Changed(Some(base)),
            _ => {
                state.clear();
                Reuse::Changed(None)
            }
        }
    }

    /// Bring cached pages up to date with the latest images of the transactions
    /// between the base and the new log start, as read by open after
    /// [`Reuse::Changed`]; `None` means they could not be read.
    pub(crate) fn refresh(&self, changes: Option<&Images>) {
        let mut state = self.0.lock().unwrap();
        let Some(changes) = changes else {
            return state.clear();
        };
        for (&index, image) in changes {
            if !state.entries.contains_key(&index) {
                continue;
            }
            let value = match image {
                Some(image) if state.decoded => page_codec::decode_bytes(image).ok(),
                image => image.clone(),
            };
            match value {
                Some(value) => state.insert(index, value),
                None => state.remove(index),
            }
        }
    }

    /// Record durable `meta` bytes, a claim beyond them, and the acknowledged
    /// commit. Cached pages must be current as of the log start of `meta`.
    pub(crate) fn record(&self, meta: Option<Page>, claim: Option<Meta>, committed: u64) {
        let mut state = self.0.lock().unwrap();
        state.base = match &meta {
            Some(bytes) => Meta::decode(bytes).ok().map(|meta| meta.log_start),
            None => Some(Meta::default().log_start),
        };
        if state.base.is_none() {
            state.clear();
        }
        state.meta = meta;
        state.claim = claim;
        state.committed = Some(committed);
    }

    /// Forget durability knowledge after a save with an unknown outcome. Cached
    /// pages stay: whatever the save did, it changed only pages of transactions
    /// after the base, which the next open refreshes if `meta` moved past them.
    pub(crate) fn forget(&self) {
        self.0.lock().unwrap().forget();
    }

    /// One past this owner's last acknowledged commit, if still known.
    pub(crate) fn committed(&self) -> Option<u64> {
        self.0.lock().unwrap().committed
    }

    /// Keep an engine for the next session; it is dropped on any reset.
    pub(crate) fn park(&self, engine: WarmEngine) {
        self.0.lock().unwrap().engine = Some(engine);
    }

    pub(crate) fn take_engine(&self) -> Option<WarmEngine> {
        self.0.lock().unwrap().engine.take()
    }

    pub(crate) fn get(&self, index: u64) -> Option<Page> {
        let mut state = self.0.lock().unwrap();
        state.clock += 1;
        let now = state.clock;
        let (bytes, used) = state.entries.get_mut(&index)?;
        let (bytes, previous) = (bytes.clone(), std::mem::replace(used, now));
        state.order.remove(&previous);
        state.order.insert(now, index);
        Some(bytes)
    }

    pub(crate) fn insert(&self, index: u64, bytes: Page) {
        self.0.lock().unwrap().insert(index, bytes);
    }

    pub(crate) fn remove(&self, index: u64) {
        self.0.lock().unwrap().remove(index);
    }
}

/// What open may reuse, given the durable `meta` it read.
#[derive(Debug, PartialEq)]
pub(crate) enum Reuse {
    /// `meta` is what this owner last saw: the claim and acknowledged commit hold.
    Known(Option<Meta>, Option<u64>),
    /// `meta` changed. With a base, cached pages changed by transactions from the
    /// base to the new log start must be refreshed before the next read; without
    /// one, the pages were dropped.
    Changed(Option<u64>),
}

impl State {
    fn insert(&mut self, index: u64, bytes: Page) {
        self.remove(index);
        if bytes.len() > self.capacity {
            return;
        }
        self.clock += 1;
        let now = self.clock;
        self.bytes += bytes.len();
        self.entries.insert(index, (bytes, now));
        self.order.insert(now, index);
        while self.bytes > self.capacity {
            let (_, oldest) = self.order.pop_first().expect("nonempty when over capacity");
            self.remove(oldest);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
        self.base = None;
    }

    fn forget(&mut self) {
        self.meta = Some(Page::new()); // matches no real record
        self.claim = None;
        self.committed = None;
        self.engine = None;
    }

    fn remove(&mut self, index: u64) {
        if let Some((bytes, used)) = self.entries.remove(&index) {
            self.bytes -= bytes.len();
            self.order.remove(&used);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/cache.rs"]
mod tests;

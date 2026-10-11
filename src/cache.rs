//! Optional owner-process state shared by successive sessions of one namespace:
//! a byte-bounded LRU of checkpointed page values plus what this owner knows to
//! be durable.
//!
//! Pages are overwritten in place, so cached values can go stale. Only the
//! process that writes them through stays coherent. The cache therefore
//! remembers the exact `meta` bytes it last read or wrote. If open reads anything
//! else, another writer checkpointed, and the cache drops everything. Without a
//! `meta` change, every page another writer could have rewritten still has a newer
//! image in the log, which overrides the cache.
//!
//! The owner also remembers facts that would otherwise cost round trips:
//! - the commit it last had acknowledged, so open need not re-put that tail;
//! - a checkpoint claim: pages through some log position were acknowledged, so the
//!   next save can advance `meta` in the same round trip as its commit, and
//!   replay can start there.
//!
//! One cache must serve exactly one namespace; sharing it returns wrong pages.
use crate::{WarmEngine, log::Meta, page_codec::Page};
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
        let meta = match &state.meta {
            Some(bytes) => Meta::decode(bytes).ok()?,
            None => Meta::default(),
        };
        Some(state.claim.map_or(meta.log_start, |claim| claim.log_start))
    }

    /// If `meta` is the durable record this owner last saw, return the claim and
    /// acknowledged commit that remain valid. Otherwise drop everything; the
    /// caller records the new `meta` once it knows it is durable.
    pub(crate) fn validate(&self, meta: Option<&[u8]>) -> Option<(Option<Meta>, Option<u64>)> {
        let mut state = self.0.lock().unwrap();
        if state.meta.as_deref() == meta {
            return Some((state.claim, state.committed));
        }
        state.entries.clear();
        state.order.clear();
        state.bytes = 0;
        state.forget();
        None
    }

    /// Record durable `meta` bytes, a claim beyond them, and the acknowledged commit.
    pub(crate) fn record(&self, meta: Option<Page>, claim: Option<Meta>, committed: u64) {
        let mut state = self.0.lock().unwrap();
        state.meta = meta;
        state.claim = claim;
        state.committed = Some(committed);
    }

    /// Forget durability knowledge after a save with an unknown outcome; the next
    /// open then resets the pages too.
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
        let mut state = self.0.lock().unwrap();
        state.remove(index);
        if bytes.len() > state.capacity {
            return;
        }
        state.clock += 1;
        let now = state.clock;
        state.bytes += bytes.len();
        state.entries.insert(index, (bytes, now));
        state.order.insert(now, index);
        while state.bytes > state.capacity {
            let (_, oldest) = state
                .order
                .pop_first()
                .expect("nonempty when over capacity");
            state.remove(oldest);
        }
    }

    pub(crate) fn remove(&self, index: u64) {
        self.0.lock().unwrap().remove(index);
    }
}

impl State {
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

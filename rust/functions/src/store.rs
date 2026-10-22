//! Where a scope's runs and KV live: a state (the `state` crate) in a key/value
//! store (in Restate, the object's state; in tests, a `MemoryKv`). A turn is
//! one flush of it: the run's progress and each participant's in records, the
//! pages the turn changed in the memory file, and the KV writes as SQL, in
//! one put.
//! Writes may land in any order; a write counts once it is acknowledged.
use crate::extension::Extensions;
use anyhow::Result;
use state::{PageCache, State, StateOptions, kv::KvStore};
use std::sync::Arc;

/// A scope's storage: the store its database lives in, and, for a process
/// that keeps owning the scope, its owner cache (a flush then needs no read
/// of what it wrote, and an open is one get).
#[derive(Clone)]
pub struct Scope {
    store: Arc<dyn KvStore>,
    /// The process's cache, and the scope's namespace in it.
    cache: Option<(PageCache, String)>,
    pub(crate) extensions: Extensions,
}

impl Scope {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self {
            store,
            cache: None,
            extensions: Extensions::new(),
        }
    }

    /// The same scope, its runs offered `extensions`.
    pub fn extended(self, extensions: Extensions) -> Self {
        Self { extensions, ..self }
    }

    /// The same scope, kept warm in `cache` between attempts, as
    /// `namespace` (see [`state::Open::cached`]).
    pub fn cached(self, cache: &PageCache, namespace: &str) -> Self {
        Self {
            cache: Some((cache.clone(), namespace.to_owned())),
            ..self
        }
    }

    /// Open the scope's state: its SQL, memory and records durable together
    /// at each turn's flush.
    pub(crate) async fn open(&self) -> Result<State> {
        let options = StateOptions {
            // A turn changes a few pages: a small inline log keeps the record
            // each turn rewrites small.
            inline_log_bytes: 1024,
            ..StateOptions::default()
        };
        let open = State::open(self.store.clone()).options(options);
        let open = match &self.cache {
            Some((cache, namespace)) => open.cached(cache, namespace),
            None => open,
        };
        open.await.map_err(storage_error)
    }
}

/// An error of the scope's storage (its state, or SQL on it).
pub(crate) fn storage_error(error: Box<dyn std::error::Error + Send + Sync>) -> anyhow::Error {
    anyhow::anyhow!(error)
}

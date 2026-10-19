//! Where a scope's runs and KV live: a sqlite-kv database in a key/value
//! store (in Restate, the object's state; in tests, a `MemoryKv`). A turn is
//! one commit of it: the run's state as the record's effects, the pages the
//! turn changed as a value of its chain, and the KV writes as SQL, in one put.
//! Writes may land in any order; a write counts once it is acknowledged.
use anyhow::Result;
use sqlite_kv::{Durability, NamespaceCache, Session, StorageOptions, kv::KvStore};
use std::sync::Arc;

/// A scope's storage: the store its database lives in, and, for a process
/// that keeps owning the scope, its owner cache (a commit then needs no read
/// of what it wrote, and an open is one get).
#[derive(Clone)]
pub struct Scope {
    store: Arc<dyn KvStore>,
    cache: Option<NamespaceCache>,
}

impl Scope {
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self { store, cache: None }
    }

    /// The same scope, kept in `cache` between attempts.
    pub fn cached(self, cache: NamespaceCache) -> Self {
        Self {
            cache: Some(cache),
            ..self
        }
    }

    /// Open the scope's database: each SQL commit durable when it returns.
    pub(crate) async fn open(&self) -> Result<Session> {
        let options = StorageOptions {
            cache: self.cache.clone(),
            durability: Durability::Commit,
            // A turn changes a few pages: a small inline log keeps the record
            // each turn rewrites small.
            inline_log_bytes: 1024,
            ..StorageOptions::default()
        };
        Session::open_with_options(self.store.clone(), options)
            .await
            .map_err(|error| anyhow::anyhow!(error))
    }
}

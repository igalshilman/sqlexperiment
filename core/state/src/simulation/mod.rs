//! Opt-in in-memory simulation, fault injection and measurement support.
//! Enabled by the `simulation` Cargo feature; excluded from the default library.
mod blobs;
mod fault;
mod latency;
mod memory;
mod monitor;
pub use blobs::MemoryBlobs;
pub use fault::{Effect, Event, Failpoint, FaultKv, Operation};
pub use latency::LatencyKv;
pub use memory::{MemoryKv, Stats, Traffic, object_kind};
pub use monitor::MonitorKv;

use crate::{Open, PageCache, PageCacheConfig, State, StateOptions, kv::KvStore};
use std::sync::Arc;
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering::Relaxed},
};

/// Cache settings for tests: a temporary file, and 1 MiB disk blocks.
pub fn cache_config(memory: usize, disk: usize) -> PageCacheConfig {
    PageCacheConfig {
        disk_block: 1 << 20,
        ..PageCacheConfig::new(memory, disk)
    }
}

/// Open a [`PageCache`] from any context, even inside an executor.
pub fn open_cache(config: PageCacheConfig) -> PageCache {
    std::thread::spawn(move || futures::executor::block_on(PageCache::open(config)))
        .join()
        .unwrap()
        .expect("open the page cache")
}

/// The process-wide cache that tests share: 64 MiB of memory, 256 MiB of disk.
pub fn shared_cache() -> &'static PageCache {
    static CACHE: OnceLock<PageCache> = OnceLock::new();
    CACHE.get_or_init(|| open_cache(cache_config(64 << 20, 256 << 20)))
}

/// A namespace of a page cache: what [`Open::cached`] takes, as one value.
#[derive(Clone)]
pub struct Namespace {
    pub cache: PageCache,
    pub key: String,
}

impl Namespace {
    pub fn new(cache: &PageCache, key: &str) -> Self {
        Self {
            cache: cache.clone(),
            key: key.to_owned(),
        }
    }
}

/// A namespace of [`shared_cache`] that nobody used before: a fresh owner.
pub fn fresh_namespace() -> Namespace {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    let key = format!("namespace-{}", KEYS.fetch_add(1, Relaxed));
    Namespace::new(shared_cache(), &key)
}

/// The handle of a [`fresh_namespace`].
#[cfg(test)]
pub(crate) fn fresh_namespace_cache() -> crate::cache::NamespaceCache {
    let namespace = fresh_namespace();
    namespace.cache.namespace(&namespace.key)
}

/// How a test opens a state: its options, and the namespace it keeps warm
/// in, if any.
#[derive(Clone, Default)]
pub struct Setup {
    pub options: StateOptions,
    pub cache: Option<Namespace>,
}

impl Setup {
    /// Open a state over `store` this way.
    pub fn open(&self, store: Arc<dyn KvStore>) -> Open {
        let open = State::open(store).options(self.options.clone());
        match &self.cache {
            Some(namespace) => open.cached(&namespace.cache, &namespace.key),
            None => open,
        }
    }
}

impl From<StateOptions> for Setup {
    fn from(options: StateOptions) -> Self {
        Self {
            options,
            cache: None,
        }
    }
}

/// Return to the executor once, even when every awaited future is ready.
pub(crate) async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

/// The key of blob `n`.
pub fn blob_key(n: u64) -> String {
    crate::store::blob_key(n)
}

/// The key of the claim ([`KvStore::unfenced_blobs`]).
pub const CLAIM_KEY: &str = crate::store::CLAIM_KEY;

/// The key of the sweep's dead blobs ([`State::collect_garbage`]).
pub const SWEEP_KEY: &str = crate::store::SWEEP_KEY;

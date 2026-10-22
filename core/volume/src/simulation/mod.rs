//! Opt-in in-memory simulation, fault injection and measurement support.
//! Enabled by the `simulation` Cargo feature; excluded from the default library.
mod fault;
mod latency;
mod memory;
mod monitor;
pub use fault::{Effect, Event, Failpoint, FaultKv, Operation};
pub use latency::LatencyKv;
pub use memory::{MemoryKv, Stats, Traffic, object_kind};
pub use monitor::MonitorKv;

use crate::{NamespaceCache, PageCache, PageCacheConfig, format::COMMIT, kv::KvStore, log::Commit};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering::Relaxed},
};

/// The transactions in the durable commit record's log: those committed since
/// the last checkpoint.
pub async fn logged_transactions(store: &dyn KvStore) -> std::io::Result<usize> {
    Ok(match store.get(COMMIT).await? {
        Some(bytes) => Commit::decode(&bytes)?.log.len(),
        None => 0,
    })
}

/// Cache settings for tests: a new temporary file and 1 MiB disk blocks. Files
/// left by earlier test processes are removed.
pub fn cache_config(memory: usize, disk: usize) -> PageCacheConfig {
    static DIRS: AtomicU64 = AtomicU64::new(0);
    static CLEANED: std::sync::Once = std::sync::Once::new();
    CLEANED.call_once(remove_stale_cache_files);
    let file = std::env::temp_dir().join(format!(
        "volume-cache-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Relaxed)
    ));
    PageCacheConfig {
        disk_block: 1 << 20,
        ..PageCacheConfig::new(file, memory, disk)
    }
}

fn remove_stale_cache_files() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("volume-cache-")) else {
            continue;
        };
        let pid = rest.split('-').next().unwrap_or_default();
        let running = std::path::Path::new("/proc").join(pid).exists();
        if !running && std::path::Path::new("/proc/self").exists() {
            let _ = std::fs::remove_file(entry.path());
        }
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

/// A namespace of the shared cache that nobody used before: a fresh owner.
pub fn fresh_namespace() -> NamespaceCache {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    shared_cache().namespace(&format!("namespace-{}", KEYS.fetch_add(1, Relaxed)))
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

/// The key of page `page` of the paged file numbered `file` (the first file
/// a namespace's commits wrote is 0).
pub fn page_key(file: u32, page: u64) -> String {
    crate::format::page_key(crate::log::page_id(file, page))
}

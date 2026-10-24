//! Process-wide owner state: blobs in a memory and disk cache ([foyer]),
//! shared by every namespace this process opens, plus per-namespace
//! knowledge of what this owner made durable.
//!
//! A [`PageCache`] is created once per process. Each namespace gets its own
//! [`NamespaceCache`] handle by key, so one namespace never reads another's
//! blobs.
//!
//! # Blobs
//!
//! A blob never changes once a durable root names it (see [`crate::store`]),
//! so it is cached under its number, for as long as the cache keeps it:
//! there is nothing to invalidate. A blob is cached only once a durable root
//! names it (after this owner's flush synced, or when a state read it as
//! named by the root it opened), so a crashed flush's blobs, which the next
//! flush overwrites, are never cached. Keys are the namespace's identity, its
//! database's (random, in the root: a namespace emptied and made again
//! numbers its blobs from 0 again, under another) and the number; a
//! namespace's identity comes from a process-wide counter, and the disk tier
//! starts empty in every process.
//!
//! # Durability knowledge
//!
//! The namespace remembers the root it last read or wrote, its last durable
//! flush (so a state can tell another flushed since it opened), and the values
//! parked at that flush (SQL's engine, say), for the next state. A flush whose outcome is unknown leaves writes this owner can read
//! but a crash can still lose, so the next open syncs first
//! ([`NamespaceCache::unsettled`]): what it reads, and caches, is durable.
//!
//! Namespace states are kept for a bounded number of namespaces, and parked
//! values within a byte budget, least recently used first; forgetting either
//! only makes a namespace cold.
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, Cache, CacheBuilder, Code, DeviceBuilder, FileDeviceBuilder, HybridCache,
    HybridCacheBuilder, RecoverMode, Spawner,
};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

/// Bookkeeping bytes charged per cached blob.
const ENTRY_OVERHEAD: usize = 64;

/// Sizes and location of a [`PageCache`].
#[derive(Clone, Debug)]
pub struct PageCacheConfig {
    /// File of the disk tier, its previous contents discarded; `None` for a
    /// temporary one, which goes away with the process.
    pub file: Option<PathBuf>,
    /// Bytes of blobs kept in memory.
    pub memory: usize,
    /// Bytes of the disk tier.
    pub disk: usize,
    /// Bytes per disk block, the unit the disk tier writes and reclaims.
    pub disk_block: usize,
    /// Namespaces whose durability knowledge is kept.
    pub namespaces: usize,
    /// Bytes of the values parked for the next session of their
    /// namespace (see [`crate::State::park`]), as they weigh it: SQL's
    /// engine, say. Least recently parked go first.
    pub parked_bytes: usize,
}

impl PageCacheConfig {
    /// `memory` bytes of blobs in memory, and `disk` in a temporary file.
    pub fn new(memory: usize, disk: usize) -> Self {
        Self {
            file: None,
            memory,
            disk,
            disk_block: 4 << 20,
            namespaces: 1 << 16,
            parked_bytes: 64 << 20,
        }
    }
}

/// Process-wide blob cache; clones share it.
///
/// ParkedValue state lives here and may refer back to its namespace's handle, so
/// handles only reach it weakly: dropping the last `PageCache` drops it.
#[derive(Clone)]
pub struct PageCache {
    shared: Arc<Shared>,
    parked: Arc<Mutex<Parked>>,
}

struct Shared {
    blobs: HybridCache<BlobKey, Bytes>,
    namespaces: Cache<String, Arc<Mutex<Owner>>>,
    /// Every namespace state a handle still holds, so that one namespace never
    /// has two states while a session uses it, even after LRU eviction.
    live: Mutex<HashMap<String, Weak<Mutex<Owner>>>>,
    /// The namespace states with a flush whose outcome is unknown, kept until
    /// a sync settles it: an evicted one would forget that its next open
    /// must sync.
    unsettled: Mutex<Vec<Arc<Mutex<Owner>>>>,
    /// Source of namespace state identities.
    ids: AtomicU64,
    /// Keeps the disk tier's own runtime alive when the host has none.
    _runtime: Option<Spawner>,
}

impl PageCache {
    /// Open the cache. Disk I/O runs on the caller's tokio runtime if there is
    /// one, otherwise on a small runtime of its own.
    pub async fn open(config: PageCacheConfig) -> io::Result<Self> {
        let spawner = match tokio::runtime::Handle::try_current() {
            Ok(handle) => Spawner::from(handle),
            Err(_) => Spawner::from(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("page-cache")
                    .enable_all()
                    .build()?,
            ),
        };
        static TEMPORARY: AtomicU64 = AtomicU64::new(0);
        let file = config.file.clone().unwrap_or_else(|| {
            let n = TEMPORARY.fetch_add(1, Relaxed);
            std::env::temp_dir().join(format!("state-cache-{}-{n}", std::process::id()))
        });
        let device = FileDeviceBuilder::new(&file)
            .with_capacity(config.disk)
            .build()
            .map_err(io::Error::other)?;
        let blobs = HybridCacheBuilder::new()
            .with_name("state-blobs")
            .memory(config.memory)
            .with_weighter(|_, blob: &Bytes| blob.len() + ENTRY_OVERHEAD)
            .storage()
            .with_engine_config(BlockEngineConfig::new(device).with_block_size(config.disk_block))
            // Namespace state is not persisted, so earlier entries are unusable.
            .with_recover_mode(RecoverMode::None)
            .with_spawner(spawner.clone())
            .build()
            .await
            .map_err(io::Error::other)?;
        // The cache keeps the file open, and nothing else uses a temporary
        // one: unlinked now, it goes away with the process however it ends
        // (where unlinking an open file is refused, it stays).
        if config.file.is_none() {
            let _ = std::fs::remove_file(&file);
        }
        Ok(Self {
            shared: Arc::new(Shared {
                blobs,
                namespaces: CacheBuilder::new(config.namespaces).build(),
                live: Mutex::default(),
                unsettled: Mutex::default(),
                // Distinct per instance, so two caches sharing a disk file by
                // mistake still never share keys.
                ids: AtomicU64::new(random() >> 1),
                _runtime: matches!(spawner, Spawner::Runtime(_)).then_some(spawner),
            }),
            parked: Arc::new(Mutex::new(Parked::new(config.parked_bytes))),
        })
    }

    /// The handle for one namespace, such as one virtual object. Successive
    /// sessions of the namespace must use the same key.
    pub(crate) fn namespace(&self, key: &str) -> NamespaceCache {
        let mut live = self.shared.live.lock().unwrap();
        let owner = match self.shared.namespaces.get(key) {
            Some(entry) => entry.value().clone(),
            None => {
                let owner = live
                    .get(key)
                    .and_then(Weak::upgrade)
                    .unwrap_or_else(|| Arc::new(Mutex::new(Owner::new(&self.shared.ids))));
                self.shared.namespaces.insert(key.to_owned(), owner.clone());
                owner
            }
        };
        live.retain(|_, owner| owner.strong_count() > 0);
        live.insert(key.to_owned(), Arc::downgrade(&owner));
        drop(live);
        let id = owner.lock().unwrap().id;
        NamespaceCache {
            shared: self.shared.clone(),
            parked: Arc::downgrade(&self.parked),
            id,
            owner,
        }
    }

    /// Bytes of blobs held in memory.
    pub fn memory_bytes(&self) -> usize {
        self.shared.blobs.memory().usage()
    }
}

/// One namespace's view of a [`PageCache`]; clones share it.
#[derive(Clone)]
pub struct NamespaceCache {
    shared: Arc<Shared>,
    parked: Weak<Mutex<Parked>>,
    /// The owner's identity, which keys its blobs and parked values.
    id: u64,
    owner: Arc<Mutex<Owner>>,
}

/// Per-namespace state.
struct Owner {
    /// Identity of this state, which keys its blobs and parked values.
    id: u64,
    /// The root as last read or written by this owner; `None` when unknown,
    /// which matches no root.
    root: Option<Bytes>,
    /// The version of this owner's last durable flush.
    committed: Option<u64>,
    /// A state's flush took the right to flush ([`NamespaceCache::claim`])
    /// and has not ended: its writes may be visible, and not durable yet.
    flushing: bool,
    /// Flushes with an unknown outcome so far, and how many of them a sync
    /// has settled since.
    unknown: u64,
    settled: u64,
}

impl NamespaceCache {
    fn owner(&self) -> std::sync::MutexGuard<'_, Owner> {
        self.owner.lock().unwrap()
    }

    /// Compare the root that open read (`bytes`) to what this owner last
    /// saw: whether it is the same.
    pub(crate) fn validate(&self, bytes: Option<&[u8]>) -> bool {
        let mut owner = self.owner();
        // An absent root is an empty namespace; a real root is never empty.
        if owner.root.as_deref() == Some(bytes.unwrap_or_default()) {
            return true;
        }
        let stale = self.forget_locked(&mut owner);
        // ParkedValue state may hold state handles, whose drop takes this lock.
        drop(owner);
        drop(stale);
        false
    }

    /// Record the durable root `bytes` an open read, and, if `committed`,
    /// that a flush may build on it, unless a flush runs: that one's
    /// outcome decides.
    pub(crate) fn record(&self, bytes: Option<Bytes>, committed: Option<u64>) {
        let mut owner = self.owner();
        // An absent root is a real state: an empty namespace.
        owner.root = Some(owned(&bytes.unwrap_or_default()));
        if committed.is_some() && !owner.flushing {
            owner.committed = committed;
        }
    }

    /// The claimed flush (see [`Self::claim`]) made `bytes`, at `version`,
    /// durable.
    pub(crate) fn flushed(&self, bytes: Bytes, version: u64) {
        let mut owner = self.owner();
        owner.root = Some(bytes);
        owner.committed = Some(version);
        owner.flushing = false;
    }

    /// Forget the root after a flush whose outcome is unknown.
    pub(crate) fn forget(&self) {
        let mut owner = self.owner();
        owner.flushing = false;
        owner.unknown += 1;
        let stale = self.forget_locked(&mut owner);
        drop(owner);
        drop(stale);
        let mut unsettled = self.shared.unsettled.lock().unwrap();
        if !unsettled
            .iter()
            .any(|owner| Arc::ptr_eq(owner, &self.owner))
        {
            unsettled.push(self.owner.clone());
        }
    }

    /// The flushes with an unknown outcome so far, if a sync has not settled
    /// them all, or a flush runs: their writes may be visible and yet not
    /// durable, so an open syncs before it reads (and caches).
    pub(crate) fn unsettled(&self) -> Option<u64> {
        let owner = self.owner();
        (owner.unknown != owner.settled || owner.flushing).then_some(owner.unknown)
    }

    /// A sync succeeded after the first `unknown` flushes with an unknown
    /// outcome (see [`Self::unsettled`]).
    pub(crate) fn settled(&self, unknown: u64) {
        let mut owner = self.owner();
        owner.settled = owner.settled.max(unknown);
        if owner.settled == owner.unknown {
            drop(owner);
            let mut unsettled = self.shared.unsettled.lock().unwrap();
            unsettled.retain(|owner| !Arc::ptr_eq(owner, &self.owner));
        }
    }

    /// Forget the root and what was parked at it, which the caller drops once
    /// it holds no lock.
    #[must_use]
    fn forget_locked(&self, owner: &mut Owner) -> HashMap<String, ParkedValue> {
        owner.root = None;
        owner.committed = None;
        self.take_parked()
    }

    /// Take the right to flush on top of `version`, this owner's last
    /// durable flush: refused if another state flushed since, or is
    /// flushing. A flush that lands records its version
    /// ([`Self::flushed`]); one that fails leaves it unknown
    /// ([`Self::forget`]).
    pub(crate) fn claim(&self, version: u64) -> bool {
        let mut owner = self.owner();
        let claimed = owner.committed == Some(version);
        if claimed {
            owner.committed = None;
            owner.flushing = true;
        }
        claimed
    }

    /// The version of this owner's last durable flush, if still known.
    pub(crate) fn committed(&self) -> Option<u64> {
        self.owner().committed
    }

    /// Keep a parked value for the next session; it is dropped
    /// on any reset.
    pub(crate) fn park(&self, name: &str, value: ParkedValue) {
        if let Some(parked) = self.parked.upgrade() {
            // Dropped once the lock is released.
            let evicted = parked
                .lock()
                .unwrap()
                .park((self.id, name.to_owned()), value);
            drop(evicted);
        }
    }

    /// Every value parked for this namespace, by key.
    pub(crate) fn take_parked(&self) -> HashMap<String, ParkedValue> {
        let Some(parked) = self.parked.upgrade() else {
            return HashMap::new();
        };
        parked.lock().unwrap().take_all(self.id)
    }

    fn key(&self, database: u64, blob: u64) -> BlobKey {
        BlobKey {
            namespace: self.id,
            database,
            blob,
        }
    }

    /// Blob `n` of `database`, from memory or disk. A disk error is a miss.
    pub(crate) async fn get(&self, database: u64, n: u64) -> Option<Bytes> {
        let key = self.key(database, n);
        // A hit in memory without a hop to the disk's threads.
        if let Some(entry) = self.shared.blobs.memory().get(&key) {
            return Some(entry.value().clone());
        }
        match self.shared.blobs.get(&key).await {
            Ok(entry) => entry.map(|entry| entry.value().clone()),
            Err(_) => None,
        }
    }

    /// Keep blob `n` of `database`, which a durable root names.
    pub(crate) fn insert(&self, database: u64, n: u64, value: Bytes) {
        self.shared
            .blobs
            .insert(self.key(database, n), owned(&value));
    }
}

impl Owner {
    fn new(ids: &AtomicU64) -> Self {
        Self {
            id: ids.fetch_add(1, Relaxed),
            root: None,
            committed: None,
            flushing: false,
            unknown: 0,
            settled: 0,
        }
    }
}

/// `value` in an allocation of its own. A value read from the store can be
/// a slice of a larger receive buffer (the Rust SDK's eager state is slices
/// of the invocation's start message), which the cache would otherwise keep
/// whole, uncounted, for as long as it keeps the value.
fn owned(value: &Bytes) -> Bytes {
    Bytes::copy_from_slice(value)
}

/// A random number, for identities.
pub(crate) fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos()),
    );
    hasher.finish()
}

/// A value parked at a durable version, for its namespace's
/// next session (see [`crate::State::park`]).
pub(crate) struct ParkedValue {
    pub value: Box<dyn Any + Send>,
    /// The version it corresponds to.
    pub version: u64,
    /// Its bytes, as whoever parked it weighs it.
    pub weight: usize,
}

/// Parked values within a byte budget, by owner and key, least recently
/// parked dropped first.
struct Parked {
    budget: usize,
    used: usize,
    clock: u64,
    parked: HashMap<(u64, String), (ParkedValue, u64)>,
    order: BTreeMap<u64, (u64, String)>,
}

impl Parked {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            clock: 0,
            parked: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    /// Park `value` under `key`, evicting the least recently parked to stay
    /// in the budget; one heavier than the whole budget is refused rather
    /// than evict everything. What no longer fits is returned.
    #[must_use]
    fn park(&mut self, key: (u64, String), value: ParkedValue) -> Vec<ParkedValue> {
        let mut out: Vec<ParkedValue> = self.take(&key).into_iter().collect();
        if value.weight > self.budget {
            out.push(value);
            return out;
        }
        self.clock += 1;
        self.used += value.weight;
        self.parked.insert(key.clone(), (value, self.clock));
        self.order.insert(self.clock, key);
        while self.used > self.budget {
            let (_, oldest) = self.order.pop_first().expect("nonempty when over budget");
            let (value, _) = self
                .parked
                .remove(&oldest)
                .expect("ordered state is parked");
            self.used -= value.weight;
            out.push(value);
        }
        out
    }

    fn take(&mut self, key: &(u64, String)) -> Option<ParkedValue> {
        let (value, parked) = self.parked.remove(key)?;
        self.order.remove(&parked);
        self.used -= value.weight;
        Some(value)
    }

    /// Every value parked by the owner `id`.
    fn take_all(&mut self, id: u64) -> HashMap<String, ParkedValue> {
        let keys: Vec<_> = (self.parked.keys())
            .filter(|(owner, _)| *owner == id)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| Some((key.1.clone(), self.take(&key)?)))
            .collect()
    }
}

/// A blob's key: its namespace's identity, its database's, and its number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BlobKey {
    namespace: u64,
    database: u64,
    blob: u64,
}

impl Code for BlobKey {
    fn encode(&self, writer: &mut impl std::io::Write) -> foyer::Result<()> {
        self.namespace.encode(writer)?;
        self.database.encode(writer)?;
        self.blob.encode(writer)
    }

    fn decode(reader: &mut impl std::io::Read) -> foyer::Result<Self> {
        Ok(Self {
            namespace: u64::decode(reader)?,
            database: u64::decode(reader)?,
            blob: u64::decode(reader)?,
        })
    }

    fn estimated_size(&self) -> usize {
        24
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/cache.rs"]
mod tests;

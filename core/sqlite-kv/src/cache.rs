//! Process-wide owner state: checkpointed pages in a memory and disk cache
//! ([foyer]), shared by every namespace this process owns, plus per-namespace
//! knowledge of what this owner made durable.
//!
//! A [`PageCache`] is created once per process. Each namespace gets its own
//! [`NamespaceCache`] handle by key, so one namespace never reads another's pages.
//!
//! # Pages
//!
//! Pages are overwritten in place, so cached values can go stale. Page keys
//! change only in checkpoints, and a checkpoint that lands moves the commit
//! record to a new *epoch*. The namespace keeps the epoch its cached pages
//! belong to: until it changes, a page key changed only by a checkpoint whose
//! record did not land, which wrote images the log still holds, and the log's
//! images override the cache. This owner updates the cache as it checkpoints;
//! an open that finds another epoch drops every page.
//!
//! The namespace also remembers the exact commit record it last read or wrote.
//! If open reads the same bytes, nothing changed since: the log, the version
//! this owner committed and an idle engine all still hold.
//!
//! # Keys are never reused
//!
//! The disk tier writes entries in the background, and a delete does not stop a
//! write already queued. So a page is never changed or invalidated under its
//! key. A key is (generation, page, version): replacing or invalidating a page
//! bumps its version, and dropping all of a namespace's pages starts a new
//! generation. Stale entries are unreachable and age out. Generations come from a
//! process-wide counter, and the disk tier starts empty in every process, so keys
//! never collide across namespaces or runs.
//!
//! # Durability knowledge
//!
//! Besides pages, the namespace remembers its last durable commit (so a
//! session can tell another committed since it opened) and an idle engine at
//! that commit, for the next session.
//!
//! Namespace states are kept for a bounded number of namespaces, and parked
//! engines within a byte budget, least recently used first; forgetting either
//! only makes a namespace cold.
use crate::{
    WarmEngine,
    hint::Usage,
    page_codec::{self, Page},
};
use foyer::{
    BlockEngineConfig, Cache, CacheBuilder, Code, DeviceBuilder, FileDeviceBuilder, HybridCache,
    HybridCacheBuilder, RecoverMode, Spawner,
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

/// Pages a namespace may have replaced or invalidated before it starts a new
/// generation, which bounds its version map.
const MAX_VERSIONED_PAGES: usize = 1 << 16;
/// Bookkeeping bytes charged per cached page.
const ENTRY_OVERHEAD: usize = 64;

/// Sizes and location of a [`PageCache`].
#[derive(Clone, Debug)]
pub struct PageCacheConfig {
    /// File of the disk tier. Its previous contents are discarded.
    pub file: PathBuf,
    /// Bytes of pages kept in memory.
    pub memory: usize,
    /// Bytes of the disk tier.
    pub disk: usize,
    /// Bytes per disk block, the unit the disk tier writes and reclaims.
    pub disk_block: usize,
    /// Cache decoded 4096-byte pages rather than stored encodings: hits need no
    /// decoding, but fewer pages fit.
    pub decoded: bool,
    /// Namespaces whose durability knowledge is kept.
    pub namespaces: usize,
    /// Bytes of idle engines kept for the next session of their namespace, by
    /// estimate: a parked engine holds its parsed schema, about 48 KiB plus 16
    /// bytes per byte of schema SQL. Least recently parked go first.
    pub engine_bytes: usize,
}

impl PageCacheConfig {
    pub fn new(file: impl Into<PathBuf>, memory: usize, disk: usize) -> Self {
        Self {
            file: file.into(),
            memory,
            disk,
            disk_block: 4 << 20,
            decoded: false,
            namespaces: 1 << 16,
            engine_bytes: 64 << 20,
        }
    }
}

/// Process-wide page cache; clones share it.
///
/// Parked engines live here and refer back to their namespace's handle, so
/// handles only reach them weakly: dropping the last `PageCache` drops them.
#[derive(Clone)]
pub struct PageCache {
    shared: Arc<Shared>,
    engines: Arc<Mutex<Engines>>,
}

struct Shared {
    decoded: bool,
    pages: HybridCache<PageKey, Page>,
    namespaces: Cache<String, Arc<Mutex<Owner>>>,
    /// Every namespace state a handle still holds, so that one namespace never
    /// has two states while a session uses it, even after LRU eviction.
    live: Mutex<HashMap<String, Weak<Mutex<Owner>>>>,
    /// Source of namespace state identities and page generations.
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
        let device = FileDeviceBuilder::new(&config.file)
            .with_capacity(config.disk)
            .build()
            .map_err(io::Error::other)?;
        let pages = HybridCacheBuilder::new()
            .with_name("sqlite-kv-pages")
            .memory(config.memory)
            .with_weighter(|_, page: &Page| page.len() + ENTRY_OVERHEAD)
            .storage()
            .with_engine_config(BlockEngineConfig::new(device).with_block_size(config.disk_block))
            // Namespace state is not persisted, so earlier entries are unusable.
            .with_recover_mode(RecoverMode::None)
            .with_spawner(spawner.clone())
            .build()
            .await
            .map_err(io::Error::other)?;
        Ok(Self {
            shared: Arc::new(Shared {
                decoded: config.decoded,
                pages,
                namespaces: CacheBuilder::new(config.namespaces).build(),
                live: Mutex::default(),
                // Distinct per instance, so two caches sharing a disk file by
                // mistake still never share keys.
                ids: AtomicU64::new(random_start()),
                _runtime: matches!(spawner, Spawner::Runtime(_)).then_some(spawner),
            }),
            engines: Arc::new(Mutex::new(Engines::new(config.engine_bytes))),
        })
    }

    /// The handle for one namespace, such as one virtual object. Successive
    /// sessions of the namespace must use the same key.
    pub fn namespace(&self, key: &str) -> NamespaceCache {
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
        NamespaceCache {
            shared: self.shared.clone(),
            engines: Arc::downgrade(&self.engines),
            owner,
        }
    }

    /// Bytes of pages held in memory.
    pub fn memory_bytes(&self) -> usize {
        self.shared.pages.memory().usage()
    }
}

/// One namespace's view of a [`PageCache`]; clones share it.
#[derive(Clone)]
pub struct NamespaceCache {
    shared: Arc<Shared>,
    engines: Weak<Mutex<Engines>>,
    owner: Arc<Mutex<Owner>>,
}

/// Per-namespace state.
struct Owner {
    /// Identity of this state, which keys its parked engine.
    id: u64,
    /// Generation of its page keys; a new one drops every page.
    generation: u64,
    /// Version of each page replaced or invalidated in this generation.
    versions: HashMap<u64, u32>,
    /// The commit record as last read or written by this owner; `None` when
    /// unknown, which matches no record.
    commit: Option<Page>,
    /// The checkpoint epoch the cached pages belong to; `None` when unknown.
    epoch: Option<u64>,
    /// The version of this owner's last durable commit.
    committed: Option<u64>,
    /// Pages this owner's sessions read, for the working-set hint.
    usage: Usage,
    /// The hint this owner last wrote, to skip rewriting an unchanged one.
    hint: Option<Vec<u8>>,
    /// Versions of open sessions (as a multiset): each reads pages outside its
    /// log from the page keys and this cache, so a checkpoint may not write
    /// images of transactions after the oldest of them.
    readers: BTreeMap<u64, usize>,
    /// Stored log values a durable record named, by number: a number is never
    /// reused once named, so they stay valid while a record names them.
    values: crate::log::Values,
}

impl NamespaceCache {
    fn owner(&self) -> std::sync::MutexGuard<'_, Owner> {
        self.owner.lock().unwrap()
    }

    pub(crate) fn is_decoded(&self) -> bool {
        self.shared.decoded
    }

    /// Compare the commit record that open read (`bytes`, in epoch `epoch`)
    /// to what this owner last saw: whether it is the same. Another epoch
    /// drops every cached page.
    pub(crate) fn validate(&self, bytes: Option<&[u8]>, epoch: u64) -> bool {
        let mut owner = self.owner();
        // An absent record is an empty namespace; a real record is never empty.
        if owner.commit.as_deref() == Some(bytes.unwrap_or_default()) {
            return true;
        }
        self.forget_locked(&mut owner);
        if owner.epoch != Some(epoch) {
            owner.clear(&self.shared.ids);
        }
        false
    }

    /// Whether cached pages belong to some epoch: this owner has seen the
    /// namespace.
    pub(crate) fn knows_pages(&self) -> bool {
        self.owner().epoch.is_some()
    }

    /// Whether cached pages belong to `epoch`.
    pub(crate) fn knows_epoch(&self, epoch: u64) -> bool {
        self.owner().epoch == Some(epoch)
    }

    /// Record the durable commit record `bytes`, in epoch `epoch`, and this
    /// owner's durable commit, if any. Cached pages must be current as of
    /// that epoch.
    pub(crate) fn record(&self, bytes: Option<Page>, epoch: u64, committed: Option<u64>) {
        let mut owner = self.owner();
        if owner.epoch != Some(epoch) {
            owner.clear(&self.shared.ids);
            owner.epoch = Some(epoch);
        }
        // An absent record is a real state: an empty namespace.
        owner.commit = Some(bytes.unwrap_or_default());
        if committed.is_some() {
            owner.committed = committed;
        }
    }

    /// Record this owner's checkpoint: the durable record `bytes`, at the
    /// next epoch after the one the cached pages belong to, which keep it.
    /// Only the pages of the log it checkpointed changed, and the caller
    /// replaces those; any other page is the same in both epochs.
    pub(crate) fn checkpointed(&self, bytes: Page, epoch: u64, committed: u64) {
        let mut owner = self.owner();
        if owner.epoch.is_some_and(|known| known + 1 == epoch) {
            owner.epoch = Some(epoch);
        }
        drop(owner);
        self.record(Some(bytes), epoch, Some(committed));
    }

    /// Forget the commit record after a commit or checkpoint whose outcome is
    /// unknown. Cached pages stay: a checkpoint that landed moved the epoch,
    /// which the next open sees.
    pub(crate) fn forget(&self) {
        self.forget_locked(&mut self.owner());
    }

    /// The stored log values this owner holds, of those in `named`.
    pub(crate) fn values(&self, named: &std::collections::BTreeSet<u64>) -> crate::log::Values {
        let owner = self.owner();
        (owner.values.iter())
            .filter(|(n, _)| named.contains(n))
            .map(|(&n, value)| (n, value.clone()))
            .collect()
    }

    /// Keep the stored log values a durable record names, `named`: those held
    /// already, and `values`.
    pub(crate) fn keep_values(
        &self,
        named: &std::collections::BTreeSet<u64>,
        values: crate::log::Values,
    ) {
        let mut owner = self.owner();
        owner.values.retain(|n, _| named.contains(n));
        owner
            .values
            .extend(values.into_iter().filter(|(n, _)| named.contains(n)));
    }

    /// Register an open session at version `next`; see [`Reader`].
    pub(crate) fn reader(&self, next: u64) -> Reader {
        *self.owner().readers.entry(next).or_default() += 1;
        Reader {
            cache: self.clone(),
            next,
        }
    }

    /// The oldest position of an open session other than one at `own`: a
    /// checkpoint may write only images of transactions that end at or before it.
    pub(crate) fn oldest_reader(&self, own: u64) -> Option<u64> {
        let owner = self.owner();
        let mut readers = owner.readers.iter();
        let (&oldest, &count) = readers.next()?;
        if oldest != own || count > 1 {
            return Some(oldest);
        }
        readers.next().map(|(&next, _)| next)
    }

    fn forget_locked(&self, owner: &mut Owner) {
        owner.commit = None;
        owner.committed = None;
        self.take_engine_of(owner.id);
    }

    /// Record a page read by a session, for the working-set hint.
    pub(crate) fn record_use(&self, page: u64, content: &[u8]) {
        self.owner().usage.record(page, content);
    }

    /// The working-set hint to write, unless this owner already wrote it.
    pub(crate) fn hint(&self, limit: usize) -> Option<Vec<u8>> {
        let owner = self.owner();
        let hint = crate::hint::encode(&owner.usage.top(limit));
        (owner.hint.as_ref() != Some(&hint)).then_some(hint)
    }

    /// Remember the hint this owner wrote.
    pub(crate) fn wrote_hint(&self, hint: Vec<u8>) {
        self.owner().hint = Some(hint);
    }

    /// The version of this owner's last durable commit, if still known.
    pub(crate) fn committed(&self) -> Option<u64> {
        self.owner().committed
    }

    /// Keep an engine for the next session; it is dropped on any reset.
    pub(crate) fn park(&self, engine: WarmEngine) {
        let id = self.owner().id;
        if let Some(engines) = self.engines.upgrade() {
            engines.lock().unwrap().park(id, engine);
        }
    }

    pub(crate) fn take_engine(&self) -> Option<WarmEngine> {
        self.take_engine_of(self.owner().id)
    }

    fn take_engine_of(&self, id: u64) -> Option<WarmEngine> {
        self.engines.upgrade()?.lock().unwrap().take(id)
    }

    fn key(&self, page: u64) -> PageKey {
        let owner = self.owner();
        PageKey {
            generation: owner.generation,
            page,
            version: owner.versions.get(&page).copied().unwrap_or(0),
        }
    }

    /// The cached value of a page, from memory or disk. A disk error is a miss.
    pub(crate) async fn get(&self, page: u64) -> Option<Page> {
        let key = self.key(page);
        match self.shared.pages.get(&key).await {
            Ok(entry) => entry.map(|entry| entry.value().clone()),
            Err(_) => None,
        }
    }

    /// The key a value read from the backend now would be cached under.
    pub(crate) fn token(&self, page: u64) -> PageKey {
        self.key(page)
    }

    /// Cache a value read from the backend after taking `token`, unless the
    /// page was replaced or invalidated meanwhile: the value may predate that.
    pub(crate) fn insert_read(&self, token: PageKey, value: Page) {
        let owner = self.owner();
        let current = owner.versions.get(&token.page).copied().unwrap_or(0);
        if owner.generation == token.generation && current == token.version {
            self.shared.pages.insert(token, value);
        }
    }

    fn insert(&self, page: u64, value: Page) {
        self.shared.pages.insert(self.key(page), value);
    }

    /// Cache a new value of a page, invalidating the old one.
    pub(crate) fn replace(&self, page: u64, value: Page) {
        let value = match self.is_decoded() {
            true => match page_codec::decode_bytes(&value) {
                Ok(page) => page,
                Err(_) => return self.remove(page),
            },
            false => value,
        };
        self.bump(page);
        self.insert(page, value);
    }

    /// Invalidate a page's cached value.
    pub(crate) fn remove(&self, page: u64) {
        let old = self.key(page);
        self.bump(page);
        self.shared.pages.remove(&old);
    }

    fn bump(&self, page: u64) {
        let mut owner = self.owner();
        if owner.versions.len() >= MAX_VERSIONED_PAGES {
            owner.clear(&self.shared.ids);
        }
        *owner.versions.entry(page).or_default() += 1;
    }

    /// The generation of its page keys; it changes when every page is dropped.
    #[cfg(any(test, feature = "simulation"))]
    pub fn generation(&self) -> u64 {
        self.owner().generation
    }
}

impl Owner {
    fn new(ids: &AtomicU64) -> Self {
        Self {
            id: ids.fetch_add(1, Relaxed),
            generation: ids.fetch_add(1, Relaxed),
            versions: HashMap::new(),
            commit: None,
            epoch: None,
            committed: None,
            usage: Usage::default(),
            hint: None,
            readers: BTreeMap::new(),
            values: crate::log::Values::new(),
        }
    }

    /// Drop every page by starting a new generation.
    fn clear(&mut self, ids: &AtomicU64) {
        self.generation = ids.fetch_add(1, Relaxed);
        self.versions.clear();
        self.epoch = None;
    }
}

/// A random starting point for identities, leaving room to count up.
fn random_start() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos()),
    );
    hasher.finish() >> 1
}

/// An open session's registration at its version; dropping it ends it.
pub(crate) struct Reader {
    cache: NamespaceCache,
    next: u64,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut owner = self.cache.owner();
        if let Some(count) = owner.readers.get_mut(&self.next) {
            *count -= 1;
            if *count == 0 {
                owner.readers.remove(&self.next);
            }
        }
    }
}

/// Parked engines within a byte budget, least recently parked dropped first.
struct Engines {
    budget: usize,
    used: usize,
    clock: u64,
    parked: HashMap<u64, (WarmEngine, u64)>,
    order: BTreeMap<u64, u64>,
}

impl Engines {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            clock: 0,
            parked: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    fn park(&mut self, id: u64, engine: WarmEngine) {
        self.take(id);
        self.clock += 1;
        self.used += engine.weight();
        self.parked.insert(id, (engine, self.clock));
        self.order.insert(self.clock, id);
        while self.used > self.budget {
            let (_, oldest) = self.order.pop_first().expect("nonempty when over budget");
            let (engine, _) = self
                .parked
                .remove(&oldest)
                .expect("ordered engines are parked");
            self.used -= engine.weight();
        }
    }

    fn take(&mut self, id: u64) -> Option<WarmEngine> {
        let (engine, parked) = self.parked.remove(&id)?;
        self.order.remove(&parked);
        self.used -= engine.weight();
        Some(engine)
    }
}

/// A page value's key: never reused for a different value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PageKey {
    generation: u64,
    page: u64,
    version: u32,
}

impl Code for PageKey {
    fn encode(&self, writer: &mut impl std::io::Write) -> foyer::Result<()> {
        self.generation.encode(writer)?;
        self.page.encode(writer)?;
        self.version.encode(writer)
    }

    fn decode(reader: &mut impl std::io::Read) -> foyer::Result<Self> {
        Ok(Self {
            generation: u64::decode(reader)?,
            page: u64::decode(reader)?,
            version: u32::decode(reader)?,
        })
    }

    fn estimated_size(&self) -> usize {
        20
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/cache.rs"]
mod tests;

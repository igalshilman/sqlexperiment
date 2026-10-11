//! Process-wide owner state: checkpointed pages in a memory and disk cache
//! ([foyer]), shared by every namespace this process owns, plus per-namespace
//! knowledge of what this owner made durable.
//!
//! A [`PageCache`] is created once per process. Each namespace gets its own
//! [`NamespaceCache`] handle by key, so one namespace never reads another's pages.
//!
//! # Pages
//!
//! Pages are overwritten in place, so cached values can go stale. Each namespace
//! keeps a *base*: the log start of the last durable `meta` this owner saw. A
//! cached page is current unless a committed transaction at or after the base
//! changed it, and while such a transaction is still in the replayed log, its
//! image overrides the cache. Only a checkpoint that moves `meta` past it makes
//! the cached value visible, and only another writer does that without writing
//! through this cache.
//!
//! The namespace also remembers the exact `meta` bytes it last read or wrote. If
//! open reads anything else, another writer checkpointed (or this owner's last
//! save failed). Open then reads the segments from the base to the new log start
//! and refreshes the cached pages they changed. If any is gone, or there are more
//! than one probe batch, the namespace drops all its pages instead.
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
//! The namespace also remembers facts that would otherwise cost round trips:
//! - the commit it last had acknowledged, so open need not re-put that tail;
//! - a checkpoint claim: pages through some log position were acknowledged, so the
//!   next save can advance `meta` in the same round trip as its commit, and
//!   replay can start there;
//! - an idle engine at that commit, for the next session.
//!
//! Namespace states are kept for a bounded number of namespaces, and parked
//! engines within a byte budget, least recently used first; forgetting either
//! only makes a namespace cold.
use crate::{
    WarmEngine,
    hint::Usage,
    log::{Images, Meta},
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
    /// `meta` bytes as last read or written by this owner; `None` when absent.
    meta: Option<Page>,
    /// Cached pages are current unless changed by a committed transaction at or
    /// after this log position; `None` when unknown.
    base: Option<u64>,
    /// A durable checkpoint this owner may record in `meta`.
    claim: Option<Meta>,
    /// One past this owner's last acknowledged commit.
    committed: Option<u64>,
    /// Pages this owner's sessions read, for the working-set hint.
    usage: Usage,
    /// The hint this owner last wrote, to skip rewriting an unchanged one.
    hint: Option<Vec<u8>>,
    /// Log positions of open sessions (as a multiset): each reads pages
    /// outside its log from the page keys and this cache, so a checkpoint may
    /// not write images of transactions at or after the oldest of them.
    readers: BTreeMap<u64, usize>,
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

impl NamespaceCache {
    fn owner(&self) -> std::sync::MutexGuard<'_, Owner> {
        self.owner.lock().unwrap()
    }

    pub(crate) fn is_decoded(&self) -> bool {
        self.shared.decoded
    }

    /// Where this owner expects the log to start, for a speculative probe.
    pub(crate) fn expected_start(&self) -> Option<u64> {
        let owner = self.owner();
        owner.claim.map(|claim| claim.log_start).or(owner.base)
    }

    /// Compare the durable `meta` that open read, with log start `log_start`, to
    /// what this owner last saw.
    pub(crate) fn validate(&self, meta: Option<&[u8]>, log_start: u64) -> Reuse {
        let mut owner = self.owner();
        if owner.meta.as_deref() == meta {
            return Reuse::Known(owner.claim, owner.committed);
        }
        self.forget_locked(&mut owner);
        match owner.base {
            Some(base) if base <= log_start => Reuse::Changed(Some(base)),
            _ => {
                owner.clear(&self.shared.ids);
                Reuse::Changed(None)
            }
        }
    }

    /// Bring cached pages up to date with the latest images of the transactions
    /// between the base and the new log start, as read by open after
    /// [`Reuse::Changed`]; `None` means they could not be read.
    pub(crate) fn refresh(&self, changes: Option<&Images>) {
        let Some(changes) = changes else {
            return self.owner().clear(&self.shared.ids);
        };
        for (&index, image) in changes {
            let cached = self.shared.pages.memory().contains(&self.key(index));
            match image {
                Some(image) if cached => self.replace(index, image.clone()),
                _ => self.remove(index),
            }
        }
    }

    /// Record durable `meta` bytes, a claim beyond them, and the acknowledged
    /// commit. Cached pages must be current as of the log start of `meta`.
    pub(crate) fn record(&self, meta: Option<Page>, claim: Option<Meta>, committed: u64) {
        let mut owner = self.owner();
        owner.base = match &meta {
            Some(bytes) => Meta::decode(bytes).ok().map(|meta| meta.log_start),
            None => Some(Meta::default().log_start),
        };
        if owner.base.is_none() {
            owner.clear(&self.shared.ids);
        }
        owner.meta = meta;
        owner.claim = claim;
        owner.committed = Some(committed);
    }

    /// Drop everything this process knows about the namespace: `meta`, a claim,
    /// the last commit, every cached page and the parked engine. For a host whose
    /// acknowledged writes may still be lost (a store that acknowledges writes as
    /// it orders them, and loses an unconfirmed tail when a request fails): call
    /// it before opening after a session whose writes were not confirmed, since
    /// this owner state would describe writes that may not exist.
    pub fn reset(&self) {
        let mut owner = self.owner();
        self.forget_locked(&mut owner);
        owner.clear(&self.shared.ids);
    }

    /// Forget durability knowledge after a save with an unknown outcome. Cached
    /// pages stay: whatever the save did, it changed only pages of transactions
    /// after the base, which the next open refreshes if `meta` moved past them.
    pub(crate) fn forget(&self) {
        self.forget_locked(&mut self.owner());
    }

    /// Forget the durable `meta` after checkpoint work failed, keeping the
    /// acknowledged commit at `committed`.
    pub(crate) fn forget_meta(&self, committed: u64) {
        let mut owner = self.owner();
        self.forget_locked(&mut owner);
        owner.committed = Some(committed);
    }

    /// Register an open session at log position `next`; see [`Reader`].
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
        owner.meta = Some(Page::new()); // matches no real record
        owner.claim = None;
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

    /// Remember an acknowledged hint.
    pub(crate) fn wrote_hint(&self, hint: Vec<u8>) {
        self.owner().hint = Some(hint);
    }

    /// One past this owner's last acknowledged commit, if still known.
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
            // Knows nothing yet: the first open neither probes speculatively
            // nor refreshes pages it does not have.
            meta: Some(Page::new()),
            base: None,
            claim: None,
            committed: None,
            usage: Usage::default(),
            hint: None,
            readers: BTreeMap::new(),
        }
    }

    /// Drop every page by starting a new generation.
    fn clear(&mut self, ids: &AtomicU64) {
        self.generation = ids.fetch_add(1, Relaxed);
        self.versions.clear();
        self.base = None;
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

/// An open session's registration at its log position; dropping it ends it.
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

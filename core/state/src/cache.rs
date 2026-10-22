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
//! this owner committed and what participants left idle all still hold.
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
//! session can tell another committed since it opened) and what participants
//! left idle at that commit (SQL's engine, say), for the next session.
//!
//! A commit whose outcome is unknown leaves writes this owner can read but a
//! crash can still lose. Were the next open to trust them, the cache could
//! keep pages and stored values of a history that never became durable, under
//! an epoch that another history reaches later with other pages. So that open
//! syncs first ([`NamespaceCache::unsettled`]): what it reads is durable.
//!
//! Namespace states are kept for a bounded number of namespaces, and idle
//! state within a byte budget, least recently used first; forgetting either
//! only makes a namespace cold.
use crate::{
    hint::Usage,
    page_codec::{self, Page},
};
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
    /// Bytes of what participants leave idle for the next session of their
    /// namespace (see [`crate::Participant::park`]), as they weigh it: SQL's
    /// engine, say. Least recently parked go first.
    pub idle_bytes: usize,
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
            idle_bytes: 64 << 20,
        }
    }
}

/// Process-wide page cache; clones share it.
///
/// Idle state lives here and may refer back to its namespace's handle, so
/// handles only reach it weakly: dropping the last `PageCache` drops it.
#[derive(Clone)]
pub struct PageCache {
    shared: Arc<Shared>,
    idle: Arc<Mutex<Parked>>,
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
            .with_name("state-pages")
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
            idle: Arc::new(Mutex::new(Parked::new(config.idle_bytes))),
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
            idle: Arc::downgrade(&self.idle),
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
    idle: Weak<Mutex<Parked>>,
    owner: Arc<Mutex<Owner>>,
}

/// Per-namespace state.
struct Owner {
    /// Identity of this state, which keys its idle state.
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
    /// log from the page keys and this cache, so a checkpoint waits until no
    /// other session is open.
    readers: BTreeMap<u64, usize>,
    /// Stored log values a durable record named, by number: a number is never
    /// reused once named, so they stay valid while a record names them.
    values: crate::log::Values,
    /// Commits with an unknown outcome so far, and how many of them a sync
    /// has settled since.
    unknown: u64,
    settled: u64,
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
        let stale = self.forget_locked(&mut owner);
        if owner.epoch != Some(epoch) {
            owner.clear(&self.shared.ids);
        }
        // Idle state may hold state handles, whose drop takes this lock.
        drop(owner);
        drop(stale);
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
        let mut owner = self.owner();
        owner.unknown += 1;
        let stale = self.forget_locked(&mut owner);
        drop(owner);
        drop(stale);
    }

    /// The commits with an unknown outcome so far, if a sync has not settled
    /// them all: their writes may be visible and yet not durable, so an open
    /// syncs before the cache keeps what it reads.
    pub(crate) fn unsettled(&self) -> Option<u64> {
        let owner = self.owner();
        (owner.unknown != owner.settled).then_some(owner.unknown)
    }

    /// A sync succeeded after the first `unknown` commits with an unknown
    /// outcome (see [`Self::unsettled`]).
    pub(crate) fn settled(&self, unknown: u64) {
        let mut owner = self.owner();
        owner.settled = owner.settled.max(unknown);
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

    /// Register an open session at `version`; see [`Reader`].
    pub(crate) fn reader(&self, version: u64) -> Reader {
        *self.owner().readers.entry(version).or_default() += 1;
        Reader {
            cache: self.clone(),
            version,
        }
    }

    /// Whether a session is open other than one at version `own`: then a
    /// checkpoint would overwrite pages it reads.
    pub(crate) fn other_readers(&self, own: u64) -> bool {
        let owner = self.owner();
        let open: usize = owner.readers.values().sum();
        open > usize::from(owner.readers.contains_key(&own))
    }

    /// Forget the commit record and what was idle at it, which the caller
    /// drops once it holds no lock.
    #[must_use]
    fn forget_locked(&self, owner: &mut Owner) -> HashMap<String, Idle> {
        owner.commit = None;
        owner.committed = None;
        self.take_idle_of(owner.id)
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

    /// Keep a participant's idle state for the next session; it is dropped
    /// on any reset.
    pub(crate) fn park(&self, name: &str, idle: Idle) {
        let id = self.owner().id;
        if let Some(parked) = self.idle.upgrade() {
            // Dropped once the lock is released.
            let evicted = parked.lock().unwrap().park((id, name.to_owned()), idle);
            drop(evicted);
        }
    }

    /// Every participant's idle state, by participant.
    pub(crate) fn take_idle(&self) -> HashMap<String, Idle> {
        self.take_idle_of(self.owner().id)
    }

    fn take_idle_of(&self, id: u64) -> HashMap<String, Idle> {
        let Some(parked) = self.idle.upgrade() else {
            return HashMap::new();
        };
        parked.lock().unwrap().take_all(id)
    }

    /// The key the page's value is cached under now: take it before reading
    /// the backend, for [`Self::insert_read`].
    pub(crate) fn key(&self, page: u64) -> PageKey {
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

    /// Cache a value read from the backend under `key`, taken before the
    /// read, unless the page was replaced or invalidated meanwhile: the value
    /// may predate that.
    pub(crate) fn insert_read(&self, key: PageKey, value: Page) {
        let owner = self.owner();
        let current = owner.versions.get(&key.page).copied().unwrap_or(0);
        if owner.generation == key.generation && current == key.version {
            self.shared.pages.insert(key, value);
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
    #[cfg(all(test, feature = "simulation"))]
    pub(crate) fn generation(&self) -> u64 {
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
            unknown: 0,
            settled: 0,
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
    version: u64,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut owner = self.cache.owner();
        if let Some(count) = owner.readers.get_mut(&self.version) {
            *count -= 1;
            if *count == 0 {
                owner.readers.remove(&self.version);
            }
        }
    }
}

/// What a participant left idle at a durable version, for its namespace's
/// next session (see [`crate::Participant::park`]).
pub(crate) struct Idle {
    pub value: Box<dyn Any + Send>,
    /// The version it corresponds to.
    pub version: u64,
    /// Its bytes, as the participant weighs it.
    pub weight: usize,
}

/// Idle state within a byte budget, by owner and participant, least recently
/// parked dropped first.
struct Parked {
    budget: usize,
    used: usize,
    clock: u64,
    parked: HashMap<(u64, String), (Idle, u64)>,
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

    /// Park `idle` under `key`, evicting the least recently parked to stay
    /// in the budget; one heavier than the whole budget is refused rather
    /// than evict everything. What no longer fits is returned.
    #[must_use]
    fn park(&mut self, key: (u64, String), idle: Idle) -> Vec<Idle> {
        let mut out: Vec<Idle> = self.take(&key).into_iter().collect();
        if idle.weight > self.budget {
            out.push(idle);
            return out;
        }
        self.clock += 1;
        self.used += idle.weight;
        self.parked.insert(key.clone(), (idle, self.clock));
        self.order.insert(self.clock, key);
        while self.used > self.budget {
            let (_, oldest) = self.order.pop_first().expect("nonempty when over budget");
            let (idle, _) = self
                .parked
                .remove(&oldest)
                .expect("ordered state is parked");
            self.used -= idle.weight;
            out.push(idle);
        }
        out
    }

    fn take(&mut self, key: &(u64, String)) -> Option<Idle> {
        let (idle, parked) = self.parked.remove(key)?;
        self.order.remove(&parked);
        self.used -= idle.weight;
        Some(idle)
    }

    /// Every participant's idle state of the owner `id`.
    fn take_all(&mut self, id: u64) -> HashMap<String, Idle> {
        let keys: Vec<_> = (self.parked.keys())
            .filter(|(owner, _)| *owner == id)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| Some((key.1.clone(), self.take(&key)?)))
            .collect()
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

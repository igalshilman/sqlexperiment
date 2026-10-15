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
//! `meta` also names its database with a random id. A namespace whose `meta`
//! names another database than the one its pages came from (its state was
//! emptied and written again) drops all its pages.
//!
//! # Keys are never reused
//!
//! The disk tier writes entries in the background, and a delete does not stop a
//! write already queued. So a page is never changed or invalidated under its
//! key. A key is (generation, page, version): replacing or invalidating a page
//! gives it a new version, and dropping all of a namespace's pages starts a new
//! generation. Stale entries are unreachable and age out. Generations and
//! versions both come from a process-wide counter that starts at a random
//! 63-bit number, so keys never collide across namespaces, or across the runs
//! that share a persistent disk tier.
//!
//! # Persistence
//!
//! A [`PageCacheConfig::persistent`] cache keeps its disk tier across
//! processes: it writes each page to disk as it is cached, recovers the disk
//! tier on open, and keeps each namespace's keys (generation, versions, base
//! and database id) in a small file of its own, rewritten when they change.
//! A namespace that a process has not seen yet starts from that file, as if
//! its `meta` had changed: open refreshes the pages changed since the base,
//! or drops them all. So a file that lags behind the disk tier, or behind the
//! namespace, is safe: what it names was current as of its base. Only one
//! process may use a persistent cache at a time; another one that tries
//! falls back to a temporary disk tier.
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
    log::Meta,
    page_codec::{self, Page},
};
use foyer::{
    BlockEngineConfig, Cache, CacheBuilder, Code, DeviceBuilder, FileDeviceBuilder, HybridCache,
    HybridCacheBuilder, HybridCachePolicy, RecoverMode, Spawner,
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
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
    /// File of the disk tier. Its previous contents are discarded, unless the
    /// cache is persistent.
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
    /// Keep the disk tier for the next process: see the module docs. Next to
    /// `file`, it keeps the namespaces' keys in the directory `<file>.owners`
    /// and a lock in `<file>.lock`.
    pub persistent: bool,
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
            persistent: false,
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
    /// Source of namespace state identities, page generations and versions.
    ids: AtomicU64,
    /// Where namespaces' keys are kept, for a persistent cache.
    owners: Option<Owners>,
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
        let owners = match config.persistent {
            true => Owners::lock(&config.file)?,
            false => None,
        };
        let file = match (&owners, config.persistent) {
            // Another process has the persistent cache: a temporary one of its own.
            (None, true) => {
                let mut name = config.file.clone().into_os_string();
                name.push(format!(".{}", std::process::id()));
                PathBuf::from(name)
            }
            _ => config.file.clone(),
        };
        let device = FileDeviceBuilder::new(&file)
            .with_capacity(config.disk)
            .build()
            .map_err(io::Error::other)?;
        if owners.is_none() && config.persistent {
            // Nothing else uses the temporary file: unlinked now, it goes away
            // with the process (where unlinking an open file is refused, it stays).
            let _ = std::fs::remove_file(&file);
        }
        let pages = HybridCacheBuilder::new()
            .with_name("sqlite-kv-pages")
            // A persistent cache writes each page to disk at once: a process
            // may end without a chance to write what memory holds.
            .with_policy(match owners {
                Some(_) => HybridCachePolicy::WriteOnInsertion,
                None => HybridCachePolicy::WriteOnEviction,
            })
            .memory(config.memory)
            .with_weighter(|_, page: &Page| page.len() + ENTRY_OVERHEAD)
            .storage()
            .with_engine_config(BlockEngineConfig::new(device).with_block_size(config.disk_block))
            // Without namespaces' keys, earlier entries are unreachable.
            .with_recover_mode(match owners {
                Some(_) => RecoverMode::Quiet,
                None => RecoverMode::None,
            })
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
                ids: AtomicU64::new(crate::log::random_id() >> 1),
                owners,
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
                let owner = live.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
                    let mut owner = Owner::new(&self.shared.ids);
                    if let Some(kept) = self.shared.owners.as_ref().and_then(|o| o.load(key)) {
                        owner.restore(kept);
                    }
                    Arc::new(Mutex::new(owner))
                });
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
            key: key.into(),
        }
    }

    /// Wait for the disk tier's pending writes, and close it: later pages are
    /// not cached. Dropping the last clone closes it too, without waiting.
    pub async fn close(&self) -> io::Result<()> {
        self.shared.pages.close().await.map_err(io::Error::other)
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
    key: Arc<str>,
}

/// Per-namespace state.
struct Owner {
    /// Identity of this state, which keys its parked engine.
    id: u64,
    /// Generation of its page keys; a new one drops every page.
    generation: u64,
    /// Version of each page replaced or invalidated in this generation.
    versions: HashMap<u64, u64>,
    /// The id of the database the pages are of, as of the base: zero before
    /// its first checkpoint; `None` when unknown.
    database: Option<u64>,
    /// The keys changed since they were last kept, for a persistent cache.
    unkept: bool,
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
    /// Sessions with unconfirmed writes to an ordered store.
    unconfirmed: usize,
    /// A session ended with unconfirmed writes, which may be lost: what this
    /// owner knows may describe writes that do not exist.
    doubtful: bool,
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

    /// Compare the durable `meta` that open read, of database `id` with log
    /// start `log_start`, to what this owner last saw.
    pub(crate) fn validate(&self, meta: Option<&[u8]>, id: u64, log_start: u64) -> Reuse {
        let mut owner = self.owner();
        if owner.meta.as_deref() == meta {
            return Reuse::Known(owner.claim, owner.committed);
        }
        self.forget_locked(&mut owner);
        match owner.base {
            Some(base) if base <= log_start && owner.database == Some(id) => {
                Reuse::Changed(Some(base))
            }
            _ => {
                owner.clear(&self.shared.ids);
                Reuse::Changed(None)
            }
        }
    }

    /// Drop the cached pages that the transactions between the base and the
    /// new log start changed, as read by open after [`Reuse::Changed`]; `None`
    /// means they could not be read, and drops every page.
    pub(crate) fn refresh(&self, changes: Option<&std::collections::BTreeSet<u64>>) {
        let Some(changes) = changes else {
            return self.owner().clear(&self.shared.ids);
        };
        for &index in changes {
            self.remove(index);
        }
    }

    /// Record durable `meta` bytes, a claim beyond them, and the acknowledged
    /// commit. Cached pages must be current as of the log start of `meta`.
    pub(crate) fn record(&self, meta: Option<Page>, claim: Option<Meta>, committed: u64) {
        let mut owner = self.owner();
        let durable = match &meta {
            Some(bytes) => Meta::decode(bytes).ok(),
            None => Some(Meta::default()),
        };
        let (base, database) = (durable.map(|m| m.log_start), durable.map(|m| m.id));
        if base.is_none() {
            owner.clear(&self.shared.ids);
        }
        owner.unkept |= (owner.base, owner.database) != (base, database);
        (owner.base, owner.database) = (base, database);
        owner.meta = meta;
        owner.claim = claim;
        owner.committed = Some(committed);
        self.keep(owner);
    }

    /// Keep the namespace's keys for the next process, if they changed.
    fn keep(&self, mut owner: std::sync::MutexGuard<'_, Owner>) {
        let Some(owners) = &self.shared.owners else {
            return;
        };
        if !std::mem::take(&mut owner.unkept) {
            return;
        }
        let kept = owner.kept();
        drop(owner);
        owners.save(&self.key, kept.as_ref());
    }

    /// Before a session of an ordered store opens: if an earlier session's
    /// writes are unconfirmed, drop everything this owner knows (`meta`, claim,
    /// commit, pages and the parked engine), since it may describe lost writes.
    pub(crate) fn open_ordered(&self) {
        let mut owner = self.owner();
        if !owner.doubtful && owner.unconfirmed == 0 {
            return;
        }
        owner.doubtful = false;
        owner.meta = Some(Page::new()); // matches no real record
        owner.claim = None;
        owner.committed = None;
        owner.clear(&self.shared.ids);
        let id = owner.id;
        drop(owner);
        // Dropped outside the lock: an engine's store may settle on drop.
        drop(self.take_engine_of(id));
    }

    /// A session's first write to an ordered store.
    pub(crate) fn write_unconfirmed(&self) {
        self.owner().unconfirmed += 1;
    }

    /// That session's writes were confirmed, or it ended without.
    pub(crate) fn settle(&self, confirmed: bool) {
        let mut owner = self.owner();
        owner.unconfirmed -= 1;
        owner.doubtful |= !confirmed;
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
        owner
            .versions
            .insert(page, self.shared.ids.fetch_add(1, Relaxed));
        owner.unkept = true;
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
            database: None,
            unkept: false,
            // Knows nothing yet: the first open neither probes speculatively
            // nor refreshes pages it does not have.
            meta: Some(Page::new()),
            base: None,
            claim: None,
            committed: None,
            usage: Usage::default(),
            hint: None,
            unconfirmed: 0,
            doubtful: false,
            readers: BTreeMap::new(),
        }
    }

    /// Drop every page by starting a new generation.
    fn clear(&mut self, ids: &AtomicU64) {
        self.generation = ids.fetch_add(1, Relaxed);
        self.versions.clear();
        self.base = None;
        self.database = None;
        self.unkept = true;
    }

    /// The keys to keep for the next process: `None` when no page is current.
    fn kept(&self) -> Option<Kept> {
        Some(Kept {
            generation: self.generation,
            base: self.base?,
            database: self.database?,
            versions: self.versions.iter().map(|(&p, &v)| (p, v)).collect(),
        })
    }

    /// Start from keys an earlier process kept. `meta` stays unknown, so the
    /// first open refreshes the pages changed since the base.
    fn restore(&mut self, kept: Kept) {
        self.generation = kept.generation;
        self.base = Some(kept.base);
        self.database = Some(kept.database);
        self.versions = kept.versions.into_iter().collect();
    }
}

/// A namespace's keys, as kept for the next process.
#[derive(Debug, PartialEq)]
pub(crate) struct Kept {
    generation: u64,
    base: u64,
    database: u64,
    versions: Vec<(u64, u64)>,
}

const KEPT_MAGIC: &[u8; 4] = b"SK14";

impl Kept {
    /// `SK14`, the namespace key's length u32 LE and bytes, generation, base,
    /// database id, the version count u32 LE and (page, version) pairs as u64
    /// LE, then an FNV-1a hash of all before it, u64 LE.
    fn encode(&self, key: &str) -> Vec<u8> {
        let mut out = KEPT_MAGIC.to_vec();
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        for n in [self.generation, self.base, self.database] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&(self.versions.len() as u32).to_le_bytes());
        for &(page, version) in &self.versions {
            out.extend_from_slice(&page.to_le_bytes());
            out.extend_from_slice(&version.to_le_bytes());
        }
        out.extend_from_slice(&fnv1a(&out).to_le_bytes());
        out
    }

    /// The keys in `bytes`, if they are well formed and of namespace `key`.
    fn decode(bytes: &[u8], key: &str) -> Option<Self> {
        let (body, hash) = bytes.split_last_chunk::<8>()?;
        if fnv1a(body) != u64::from_le_bytes(*hash) {
            return None;
        }
        let mut rest = body.strip_prefix(KEPT_MAGIC)?;
        let mut take = |n: usize| {
            let (head, tail) = rest.split_at_checked(n)?;
            rest = tail;
            Some(head)
        };
        let u32_le = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap()) as usize;
        let u64_le = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap());
        let len = u32_le(take(4)?);
        if take(len)? != key.as_bytes() {
            return None;
        }
        let (generation, base, database) = (u64_le(take(8)?), u64_le(take(8)?), u64_le(take(8)?));
        let count = u32_le(take(4)?);
        if count > MAX_VERSIONED_PAGES {
            return None;
        }
        let mut versions = Vec::with_capacity(count);
        for _ in 0..count {
            versions.push((u64_le(take(8)?), u64_le(take(8)?)));
        }
        if !rest.is_empty() {
            return None;
        }
        Some(Self {
            generation,
            base,
            database,
            versions,
        })
    }
}

/// FNV-1a: stable across builds, unlike the standard library's hashers.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The directory of a persistent cache's namespace keys, a file each, named
/// by a hash of the namespace key, and the lock that makes the cache this
/// process's own.
struct Owners {
    dir: PathBuf,
    _lock: std::fs::File,
    /// Distinguishes temporary files.
    writes: AtomicU64,
}

impl Owners {
    /// Take the cache at `file` for this process: `None` if another has it.
    fn lock(file: &Path) -> io::Result<Option<Self>> {
        let with = |suffix: &str| {
            let mut name = file.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        let dir = with(".owners");
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::File::create(with(".lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Self {
                dir,
                _lock: lock,
                writes: AtomicU64::new(0),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{:016x}", fnv1a(key.as_bytes())))
    }

    fn load(&self, key: &str) -> Option<Kept> {
        Kept::decode(&std::fs::read(self.path(key)).ok()?, key)
    }

    /// Replace a namespace's keys, or remove them for `None`. Best effort: a
    /// lost write leaves older keys, or none, which only makes it colder.
    fn save(&self, key: &str, kept: Option<&Kept>) {
        let path = self.path(key);
        let Some(kept) = kept else {
            let _ = std::fs::remove_file(path);
            return;
        };
        let n = self.writes.fetch_add(1, Relaxed);
        let temporary = self
            .dir
            .join(format!("{:016x}.{n}.tmp", fnv1a(key.as_bytes())));
        if std::fs::write(&temporary, kept.encode(key)).is_err()
            || std::fs::rename(&temporary, &path).is_err()
        {
            let _ = std::fs::remove_file(temporary);
        }
    }
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
    version: u64,
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
            version: u64::decode(reader)?,
        })
    }

    fn estimated_size(&self) -> usize {
        24
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/cache.rs"]
mod tests;

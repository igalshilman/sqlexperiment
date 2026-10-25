//! Transactional state over an asynchronous key/value store: paged files,
//! records and logs, changed through a mutation in memory, which a flush
//! makes durable at once.
//!
//! A [`State`] is one namespace in a KV store: the state of a Restate object
//! without Restate's journal, which keeps writes in issue order and makes
//! them durable at a sync (see [`kv`]). It needs no multi-key transaction or
//! compare-and-swap; only collecting garbage lists keys. Its [`File`]s are 4 KiB pages read
//! and written at random, its [`Record`]s whole values, and its [`Log`]s
//! entries appended and truncated from the start. The state is
//! read; it changes only through a [`Mutation`] ([`State::mutation`]): the
//! records set, pages written and entries appended there become durable
//! together at [`Mutation::flush`], and a mutation dropped discards them.
//!
//! The state knows nothing of what writes it: the `sql` crate keeps its
//! database in the file `db`, moving what it committed into a mutation
//! before the host flushes it; the durable functions runtime adds a
//! program's memory and its run. Coordinating them is theirs.
//!
//! # Copy on write
//!
//! Nothing stored is ever changed: a flush writes what changed as new,
//! immutable blobs, each named by the hash of its contents (so equal blobs
//! are stored once), then the root, the one key it overwrites, which names
//! them; then it syncs once. A file is stored as chunks of a few pages, each
//! a zstd frame compressed against the chunk's base, so a flush costs about
//! what it changed. A crash keeps a prefix of a flush's writes, so the root
//! stored names only blobs that are there: there is no log to replay, and
//! nothing to repair. See the private `store` module, and `docs/DESIGN.md`.
//!
//! | Layer | Representation | Survives? |
//! |---|---|---|
//! | A mutation | Records set, pages written and entries appended in it, in memory | No: dropping it discards them |
//! | Root | `v17/root`: the directory, small records, and the top of each file's chunk map | Yes, after an acknowledged flush |
//! | Blobs | `v17/blob/<hash>`: a chunk's base or delta, a map node, a large record, or a log entry | Yes, until a sweep finds no root a reader may hold names it |
//! | Sweep | `v17/sweep`: the blobs the last sweep found unnamed | Yes |
//!
//! # Garbage collection
//!
//! A flush deletes nothing: what no root names any more is garbage, which
//! [`State::collect_garbage`] collects by mark and sweep. It lists the
//! blobs ([`KvStore::list`]), marks those the root names, and deletes those
//! the previous sweep found unnamed too, at least
//! [`StateOptions::garbage_after`] versions ago, unless a flush named them
//! again since; the others it keeps as candidates for the next sweep. Call
//! it now and then; a store that cannot list is never swept.
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including
//! settlement of cancelled requests before reopening or handing ownership to
//! another session. There is no distributed lock or fence here.
//!
//! Any number of states may read beside the owner ([`Open::reader`], or
//! states of the owner that overlap in a [`PageCache`]): each reads the root
//! it opened, a consistent snapshot, while the owner flushes, because blobs
//! never change and are deleted only [`StateOptions::garbage_after`]
//! versions after a sweep found no root naming them. Only one flushes: a
//! flush is refused if another state of the owner flushed since this one
//! opened.
//!
//! The [`PageCache`] is the process's: it keeps blobs in memory and on a
//! local disk tier under one budget for every namespace it opens, under
//! their names, for as long as it likes: a blob never changes. It
//! remembers the root this owner made durable, so a steady-state open and
//! flush are each one round trip. A value may be parked there for the next
//! state of the namespace ([`State::park`]), such as an SQL engine, which
//! takes it back when nothing changed. The disk tier's I/O runs on tokio.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use state::{State, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> state::Result<()> {
//! let state = State::open(store).await?;
//! let mut mutation = state.mutation();
//! mutation.set(&state.record("progress"), "step 1".into())?;
//! mutation.write(&state.file("image")?, 0, Some(vec![7; 4096].into()))?;
//! mutation.flush().await?; // the record and the page, durable together
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! A file holds at most [`MAX_FILE_BYTES`], a record's value
//! [`MAX_RECORD_BYTES`] (a log entry too), a log [`MAX_LOG_ENTRIES`], a name
//! [`MAX_NAME_BYTES`]. The backend is assumed
//! to return what was stored; decoding checks structure and bounds, and
//! blobs' zstd frames carry a checksum. Use a fresh, dedicated namespace; no
//! migration or format discovery is implemented. Simulation and benchmark
//! support require the opt-in `simulation` feature.
pub mod blobs;
mod cache;
pub mod kv;
mod map;
#[cfg(feature = "simulation")]
pub mod simulation;
mod store;
mod zstd;

pub use cache::{PageCache, PageCacheConfig};
use store::Is;
pub use store::{
    Collected, FlushReport, MAX_FILE_BYTES, MAX_LOG_ENTRIES, MAX_NAME_BYTES, MAX_RECORD_BYTES,
    ROOT_KEY,
};

use crate::cache::{NamespaceCache, ParkedValue};
use crate::kv::KvStore;
use crate::store::Snapshot;
use bytes::Bytes;
use std::{
    any::Any,
    collections::HashMap,
    future::{Future, IntoFuture},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

/// State and backend errors returned to the host task.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Bytes of a page of a [`File`].
pub const PAGE_SIZE: usize = 4096;

/// A page's 4096 bytes.
pub type Page = Bytes;

/// What a paged file's absent pages hold: a golden image it diverges from (a
/// VM's memory, say). An absent page, or a hole, of a file with a base reads
/// as the base's page (zeros past it), and its chunks are compressed against
/// it until they have a base of their own.
pub trait Base: Send + Sync {
    /// Names the base: a file made with one is opened only with it again.
    /// Below `u64::MAX`.
    fn id(&self) -> u64;
    /// Page `index` (4096 bytes); `None` past the base's end.
    fn page(&self, index: u64) -> Option<&[u8]>;
}

/// Default [`StateOptions::chunk_pages`].
pub const CHUNK_PAGES: u64 = 4;

/// How a state flushes and reads. Only [`Self::chunk_pages`] and
/// [`Self::fanout`] are stored, when the namespace is made; the others never
/// change the stored format.
#[derive(Clone)]
pub struct StateOptions {
    /// Pages per chunk, the unit a flush rewrites: smaller chunks cost less
    /// to rebase, larger ones compress better and make a smaller root. Set
    /// when the namespace is made; its root keeps it.
    pub chunk_pages: u64,
    /// Entries per node of a file's chunk map, a radix tree whose top node
    /// the root holds: a file of up to this many chunks is mapped in the
    /// root alone; each level more multiplies that by it, and costs a cold
    /// read a round trip and a flush a node per path it changes. Set when
    /// the namespace is made; its root keeps it.
    pub fanout: u64,
    /// A chunk's delta past this fraction of its base's stored bytes is
    /// written as a new base instead.
    pub rebase_ratio: f64,
    /// Bytes of a record the root holds; a larger one is a blob of its own,
    /// written when it changes.
    pub inline_record_bytes: usize,
    /// Every chunk of a file of at most this many pages is fetched at open,
    /// alongside the root's other blobs.
    pub prefetch_pages: u64,
    /// Versions between the sweep that finds a blob unnamed and the sweep
    /// that deletes it (at least 1): a reader that opened before the first
    /// reads its root for as long ([`State::collect_garbage`]).
    pub garbage_after: u64,
}

impl Default for StateOptions {
    fn default() -> Self {
        Self {
            chunk_pages: CHUNK_PAGES,
            fanout: map::FANOUT,
            rebase_ratio: 0.25,
            inline_record_bytes: 1024,
            prefetch_pages: 16,
            garbage_after: 16,
        }
    }
}

/// One namespace's state: its paged files and its records, as last flushed.
/// It is read here, and changed through a [`Mutation`] ([`Self::mutation`]), which
/// [`Mutation::flush`] makes durable at once. A cheap handle; clones, and the
/// views of [`Self::scoped`], share it.
///
/// Acquire exclusive ownership of the namespace and settle the preceding
/// owner's requests before opening it; a reader ([`Open::reader`]) may open
/// beside the owner.
#[derive(Clone)]
pub struct State {
    shared: Arc<Shared>,
    /// Prefixed to every name: a view of part of the state.
    prefix: Arc<str>,
}

/// What a state's handles share.
struct Shared {
    /// The durable state: as opened, then as the mutations flushed it.
    snapshot: Mutex<Arc<Snapshot>>,
    options: StateOptions,
    cache: Option<NamespaceCache>,
    /// What was parked at the state this one opened on, by key, until
    /// taken ([`State::take_parked`]).
    parked: Mutex<HashMap<String, ParkedValue>>,
    /// A reader's ([`Open::reader`]): it never flushes.
    reader: AtomicBool,
    /// A flush failed or was cancelled: its outcome is unknown.
    poisoned: AtomicBool,
    /// A flush is running.
    flushing: AtomicBool,
}

impl Shared {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.lock().unwrap().clone()
    }
}

/// A running flush. One that fails ends the state. One dropped midway has
/// an unknown outcome too, and the cache forgets what it knew of the
/// namespace (as a failed sync makes it), so that no other state flushes on
/// top of a state that may be gone.
struct Flushing<'a> {
    shared: &'a Shared,
    /// The flush returned, rather than being dropped.
    returned: bool,
    /// It succeeded.
    done: bool,
}

impl Drop for Flushing<'_> {
    fn drop(&mut self) {
        self.shared.flushing.store(false, Ordering::SeqCst);
        if !self.done {
            self.shared.poisoned.store(true, Ordering::SeqCst);
        }
        if !self.returned
            && let Some(cache) = &self.shared.cache
        {
            cache.forget();
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // What was parked and not taken goes back, if nothing changed.
        let Some(cache) = self.cache.as_ref().filter(|_| !*self.reader.get_mut()) else {
            return;
        };
        let version = self.snapshot().version();
        for (key, value) in self.parked.get_mut().unwrap().drain() {
            if value.version == version && cache.committed() == Some(version) {
                cache.park(&key, value);
            }
        }
    }
}

/// The error of a state that a failed flush ended.
const ENDED: &str = "the state ended after a failed or cancelled flush; open a new one";

impl State {
    /// Open the namespace over `store`: read its root (and the blobs of its
    /// large records and small files) before anything reads; a crash leaves
    /// nothing to recover. Await it as it is, or set how first:
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use state::{PageCache, PageCacheConfig, State, StateOptions, kv::KvStore};
    /// # async fn example(store: Arc<dyn KvStore>, cache: PageCache) -> state::Result<()> {
    /// let state = State::open(store.clone()).await?;
    /// let state = State::open(store.clone()).cached(&cache, "object-42").await?;
    /// let state = State::open(store)
    ///     .options(StateOptions { garbage_after: 4, ..StateOptions::default() })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(store: Arc<dyn KvStore>) -> Open {
        Open {
            store,
            options: StateOptions::default(),
            cache: None,
            reader: false,
        }
    }

    async fn open_as(
        store: Arc<dyn KvStore>,
        options: StateOptions,
        cache: Option<NamespaceCache>,
        reader: bool,
    ) -> Result<Self> {
        let snapshot = Snapshot::open(store, options.clone(), cache.clone(), reader).await?;
        let mut parked = match &cache {
            Some(cache) if snapshot.unchanged => cache.take_parked(),
            _ => HashMap::new(),
        };
        parked.retain(|_, value| value.version == snapshot.version());
        Ok(Self {
            shared: Arc::new(Shared {
                snapshot: Mutex::new(Arc::new(snapshot)),
                options,
                cache,
                parked: Mutex::new(parked),
                reader: AtomicBool::new(reader),
                poisoned: AtomicBool::new(false),
                flushing: AtomicBool::new(false),
            }),
            prefix: Arc::from(""),
        })
    }

    /// The options this state opened with.
    pub fn options(&self) -> &StateOptions {
        &self.shared.options
    }

    /// The record named `name`: a whole value the root keeps, read
    /// here and set in a mutation (see [`Record`]).
    pub fn record(&self, name: &str) -> Record {
        Record {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        }
    }

    /// The log named `name`: entries appended in a mutation and truncated
    /// from its start (see [`Log`]).
    pub fn log(&self, name: &str) -> Log {
        Log {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        }
    }

    /// The paged file named `name`: 4 KiB pages, read here and written in a
    /// mutation (see [`File`]).
    pub fn file(&self, name: &str) -> Result<File> {
        Ok(File {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        })
    }

    /// The paged file named `name`, over `base` (see [`Base`]): an absent
    /// page, or a hole, reads as the base's page, and pages are compressed
    /// against it. A file made with a base opens only with that one again
    /// (by [`Base::id`]). [`Self::file`] opens it without its base: it then
    /// reads nothing, and a flush may only make whole chunks of it holes,
    /// or cut its length.
    pub fn file_with_base(&self, name: &str, base: Arc<dyn Base>) -> Result<File> {
        let file = self.file(name)?;
        let mut snapshot = self.shared.snapshot.lock().unwrap();
        *snapshot = Arc::new(snapshot.with_base(file.name(), base)?);
        Ok(file)
    }

    /// The part of the state whose names start with `prefix`, named without
    /// it: an extension's, say (`scoped("fs/")` makes `manifest` the record
    /// `fs/manifest`).
    pub fn scoped(&self, prefix: &str) -> State {
        State {
            prefix: format!("{}{prefix}", self.prefix).into(),
            ..self.clone()
        }
    }

    /// What was parked under `key` ([`Self::park`]) at the end of the
    /// owner's previous state, if nothing changed since and it is a `T`;
    /// one of another type stays parked.
    pub fn take_parked<T: Any + Send>(&self, key: &str) -> Option<T> {
        let key = format!("{}{key}", self.prefix);
        let mut parked = self.shared.parked.lock().unwrap();
        if !parked.get(&key)?.value.is::<T>() {
            return None;
        }
        let value = parked.remove(&key)?.value.downcast().ok()?;
        Some(*value)
    }

    /// Whether [`Self::park`] would keep a value now: the state has a
    /// [`PageCache`] and is at the owner's last durable flush.
    pub fn can_park(&self) -> bool {
        let Some(cache) = &self.shared.cache else {
            return false;
        };
        !self.is_reader() && self.is_usable() && cache.committed() == Some(self.version())
    }

    /// Leave `value`, which corresponds to the state as last flushed, for
    /// the namespace's next state under `key` ([`Self::take_parked`]),
    /// weighing `weight` bytes against [`PageCacheConfig::parked_bytes`]:
    /// an SQL engine, say, which the next state then need not open. `Err`
    /// gives it back if it cannot be kept (see [`Self::can_park`]). The
    /// cache drops it when anything changes, or under its budget; one
    /// heavier than the whole budget is not kept. `value` must hold no
    /// handle of this state (a [`File`], a [`Record`], the [`State`]), which
    /// would keep it open meanwhile, on a root whose blobs later flushes
    /// delete.
    pub fn park<T: Any + Send>(
        &self,
        key: &str,
        value: T,
        weight: usize,
    ) -> std::result::Result<(), T> {
        if !self.can_park() {
            return Err(value);
        }
        let cache = self.shared.cache.as_ref().unwrap();
        let parked = ParkedValue {
            value: Box::new(value),
            version: self.version(),
            weight,
        };
        cache.park(&format!("{}{key}", self.prefix), parked);
        Ok(())
    }

    /// A new, empty mutation on this state: the records set and pages written in
    /// it ([`Mutation::set`], [`Mutation::write`]) become durable together at
    /// [`Mutation::flush`]; dropping it discards them.
    pub fn mutation(&self) -> Mutation {
        Mutation {
            shared: self.shared.clone(),
            records: Default::default(),
            files: Default::default(),
            logs: Default::default(),
            receipts: Vec::new(),
            taken: false,
        }
    }

    /// The namespace's version: the flushes ever made, as this state holds
    /// it (opened on, or flushed itself).
    pub fn version(&self) -> u64 {
        self.shared.snapshot().version()
    }

    /// Whether the state opened in a [`PageCache`] ([`Open::cached`]).
    pub fn is_cached(&self) -> bool {
        self.shared.cache.is_some()
    }

    /// Whether this is a reader ([`Open::reader`]): its mutations never flush.
    pub fn is_reader(&self) -> bool {
        self.shared.reader.load(Ordering::SeqCst)
    }

    /// Whether the state can flush: a failed or cancelled flush ends it.
    pub fn is_usable(&self) -> bool {
        !self.shared.poisoned.load(Ordering::SeqCst)
    }

    /// Delete the blobs no root names: mark and sweep, in two phases. List
    /// the store's blobs, and mark those the root names; delete those that
    /// the previous sweep, at least [`StateOptions::garbage_after`] versions
    /// ago, found unnamed too, and that no flush named since; keep the
    /// others unnamed for the next sweep. So a reader that opened that many
    /// versions ago still reads its root. Call it now and then: every
    /// [`StateOptions::garbage_after`] flushes, say.
    ///
    /// A sweep that writes is a flush, at the next version: refused, and
    /// ending the state when it fails, as [`Mutation::flush`] is. One that
    /// fails before it writes (a store that cannot list its keys,
    /// [`KvStore::list`]) leaves the state as it was.
    pub async fn collect_garbage(&self) -> Result<Collected> {
        let shared = &self.shared;
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if self.is_reader() {
            return Err("a reader writes nothing".into());
        }
        if shared.flushing.swap(true, Ordering::SeqCst) {
            return Err("a flush of this state is running".into());
        }
        let mut running = Flushing {
            shared,
            returned: false,
            done: false,
        };
        let snapshot = shared.snapshot();
        let marked = match snapshot.mark().await {
            Ok(marked) => marked,
            Err(error) => {
                // Nothing written: the state goes on.
                (running.returned, running.done) = (true, true);
                return Err(error.into());
            }
        };
        let result = snapshot.sweep(marked).await;
        running.returned = true;
        let (collected, after) = result?;
        *shared.snapshot.lock().unwrap() = Arc::new(after);
        running.done = true;
        Ok(collected)
    }
}

/// How to open a [`State`] ([`State::open`]): await it to open.
#[must_use = "a state opens when awaited"]
pub struct Open {
    store: Arc<dyn KvStore>,
    options: StateOptions,
    cache: Option<NamespaceCache>,
    reader: bool,
}

impl Open {
    /// Flush and read with `options` rather than the defaults.
    pub fn options(self, options: StateOptions) -> Self {
        Self { options, ..self }
    }

    /// Keep the state warm in the process's `cache`, as `namespace`: the
    /// key of what the store holds, which no other state of this cache
    /// uses for another (an object's key, say). With nothing changed since
    /// this process last flushed it, the next open reads no pages and gets
    /// back what was parked there ([`State::park`]).
    pub fn cached(self, cache: &PageCache, namespace: &str) -> Self {
        Self {
            cache: Some(cache.namespace(namespace)),
            ..self
        }
    }

    /// Open a reader of a namespace that another owns, alongside it: a
    /// state that writes nothing to the store (its mutations refuse to flush). It
    /// reads the state as of open, whatever the owner flushes meanwhile, for
    /// [`StateOptions::garbage_after`] of its flushes. With a cache
    /// ([`Self::cached`]), it reads the blobs the owner cached, and changes
    /// nothing there.
    pub fn reader(self) -> Self {
        Self {
            reader: true,
            ..self
        }
    }
}

impl IntoFuture for Open {
    type Output = Result<State>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<State>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        let Open {
            store,
            options,
            cache,
            reader,
        } = self;
        Box::pin(State::open_as(store, options, cache, reader))
    }
}

/// A change to a [`State`]: the records set, pages written and log entries
/// appended and truncated since [`State::mutation`] made it, in memory, which
/// [`Self::flush`] makes durable together, atomically. There is no other way to change the state. A mutation
/// dropped discards its writes; whoever moved changes into it learns which
/// happened from its [`Receipt`].
pub struct Mutation {
    shared: Arc<Shared>,
    records: store::Records,
    files: store::Writes,
    logs: store::Appends,
    receipts: Vec<Receipt>,
    /// [`Self::flush`] took its writes: dropping discards nothing.
    taken: bool,
}

impl Mutation {
    /// Whether nothing was set or written.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.files.is_empty() && self.logs.is_empty()
    }

    /// A receipt for what becomes of this mutation (see [`Receipt`]).
    pub fn receipt(&mut self) -> Receipt {
        let receipt = Receipt(Arc::new(AtomicU8::new(PENDING)));
        self.receipts.push(receipt.clone());
        receipt
    }

    /// Replace `record`'s value with this mutation's flush. Refused, before the
    /// mutation changes, is what no flush could store: a value past
    /// [`MAX_RECORD_BYTES`], a name past [`MAX_NAME_BYTES`], or the name of
    /// a paged file.
    pub fn set(&mut self, record: &Record, value: Bytes) -> Result<()> {
        self.of(&record.shared)?;
        if value.len() > MAX_RECORD_BYTES {
            let (len, max) = (value.len(), MAX_RECORD_BYTES);
            return Err(format!("record {:?} of {len} bytes exceeds {max}", record.name).into());
        }
        self.claim(&record.name, Is::Record)?;
        self.records.insert(record.name.to_string(), value);
        Ok(())
    }

    /// Write `image` (4096 bytes) at `file`'s `page`, or a hole with `None`,
    /// with this mutation's flush, which grows the file to include it if it is
    /// shorter then. Refused, before the mutation changes, is what no flush could
    /// store: a page past the largest file, a name past [`MAX_NAME_BYTES`],
    /// or the name of a record.
    pub fn write(&mut self, file: &File, page: u64, image: Option<Page>) -> Result<()> {
        if image.as_ref().is_some_and(|image| image.len() != PAGE_SIZE) {
            return Err("a page is 4096 bytes".into());
        }
        let end = page.saturating_add(1).saturating_mul(PAGE);
        if end > MAX_FILE_BYTES {
            return Err("a page past the largest file".into());
        }
        self.change(file, |written| {
            written.size = written.size.max(end);
            written.pages.insert(page, image);
        })
    }

    /// Make `file` `len` bytes, a multiple of 512, with this mutation's flush,
    /// whatever its length is then: pages wholly past it become holes, so
    /// regrowing it reads zeros (or its base's pages). Bytes of a last,
    /// partial page past `len` stay as written.
    pub fn set_len(&mut self, file: &File, len: u64) -> Result<()> {
        if !len.is_multiple_of(512) || len > MAX_FILE_BYTES {
            return Err(format!("invalid file length {len}").into());
        }
        self.change(file, |written| {
            // Every page past it a hole: those written in the mutation too, as an
            // earlier shrink may have cut flushed pages there.
            let kept = len.div_ceil(PAGE);
            for (_, image) in written.pages.range_mut(kept..) {
                *image = None;
            }
            for page in kept..written.size.div_ceil(PAGE) {
                written.pages.insert(page, None);
            }
            written.size = len;
            written.sized = true;
        })
    }

    /// Change `file`'s writes with `f`, starting from none at its length as
    /// last flushed.
    fn change(&mut self, file: &File, f: impl FnOnce(&mut store::Written)) -> Result<()> {
        self.of(&file.shared)?;
        self.claim(&file.name, Is::Paged)?;
        let size = file.len();
        let written = (self.files.entry(file.name.to_string())).or_insert_with(|| store::Written {
            size,
            pages: Default::default(),
            sized: false,
        });
        f(written);
        Ok(())
    }

    /// Append `entry` to `log` with this mutation's flush: its index, one
    /// past the last entry the log holds or this mutation appended. Refused,
    /// before the mutation changes, is what no flush could store: an entry
    /// past [`MAX_RECORD_BYTES`], a log past [`MAX_LOG_ENTRIES`], a name
    /// past [`MAX_NAME_BYTES`], or the name of a record or a paged file. The
    /// flush is refused if another appended to the log since: the index
    /// would no longer hold.
    pub fn append(&mut self, log: &Log, entry: Bytes) -> Result<u64> {
        self.of(&log.shared)?;
        if entry.len() > MAX_RECORD_BYTES {
            let (len, max) = (entry.len(), MAX_RECORD_BYTES);
            return Err(format!("{:?}: an entry of {len} bytes exceeds {max}", log.name).into());
        }
        self.claim(&log.name, Is::Log)?;
        let (first, held_next) = log.bounds();
        let mine = self.logs.get(&*log.name);
        let next = held_next + mine.map_or(0, |mine| mine.values.len() as u64);
        let start = mine.map_or(0, |mine| mine.before).max(first);
        if next + 1 - start > MAX_LOG_ENTRIES {
            let max = MAX_LOG_ENTRIES;
            return Err(format!("{:?} would hold more than {max} entries", log.name).into());
        }
        let appended = self.logs.entry(log.name.to_string()).or_default();
        if appended.values.is_empty() {
            appended.from = held_next;
        }
        appended.values.push(entry);
        Ok(next)
    }

    /// Drop `log`'s entries before index `before` with this mutation's
    /// flush, those it appends included: after it, the log starts at
    /// `before`, or where it started if that is later. Refused is an
    /// index past the log's end, this mutation's appends included. One that
    /// drops nothing changes nothing.
    pub fn truncate(&mut self, log: &Log, before: u64) -> Result<()> {
        self.of(&log.shared)?;
        self.claim(&log.name, Is::Log)?;
        let (first, next) = log.bounds();
        let mine = self.logs.get(&*log.name);
        let appended = mine.map_or(0, |mine| mine.values.len() as u64);
        if before > next + appended {
            return Err(format!("{:?}: truncating past its end", log.name).into());
        }
        if before <= mine.map_or(0, |mine| mine.before).max(first) {
            return Ok(());
        }
        self.logs.entry(log.name.to_string()).or_default().before = before;
        Ok(())
    }

    /// Refuse `name` as `wanted` if it is another kind, in the state or in
    /// this mutation, or a name no flush could store.
    fn claim(&self, name: &str, wanted: Is) -> Result<()> {
        check_name(name)?;
        let mine = (self.records.contains_key(name).then_some(Is::Record))
            .or(self.files.contains_key(name).then_some(Is::Paged))
            .or(self.logs.contains_key(name).then_some(Is::Log));
        match mine.or(self.shared.snapshot().is(name)) {
            Some(held) if held != wanted => Err(held.refuse(name, wanted).into()),
            _ => Ok(()),
        }
    }

    /// Make every record set, page written and entry appended durable: the
    /// blobs of the chunks, records and entries it changes, then the root
    /// that names them, then one sync, in one round trip. Nothing changed:
    /// nothing is written. It is refused while another flush of the state
    /// runs, and for writes the state cannot store; a refused flush writes
    /// nothing, and the state goes on. `Ok`
    /// holds what it wrote; any other error (one refused because another
    /// state flushed since included), or dropping the future, ends the
    /// state: open it again.
    pub async fn flush(mut self) -> Result<FlushReport> {
        let shared = self.shared.clone();
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if shared.reader.load(Ordering::SeqCst) {
            return Err("a reader writes nothing".into());
        }
        let snapshot = shared.snapshot();
        // What the writes checked as they were made still holds, unless
        // another mutation flushed since: refused before anything is written.
        snapshot
            .check(&self.files, &self.records, &self.logs)
            .await?;
        if shared.flushing.swap(true, Ordering::SeqCst) {
            return Err("a flush of this state is running".into());
        }
        let mut running = Flushing {
            shared: &shared,
            returned: false,
            done: false,
        };
        let files = std::mem::take(&mut self.files);
        let records = std::mem::take(&mut self.records);
        let logs = std::mem::take(&mut self.logs);
        self.taken = true;
        let result = snapshot.commit(files, records, logs).await;
        running.returned = true;
        let (report, after) = result?;
        *shared.snapshot.lock().unwrap() = Arc::new(after);
        running.done = true;
        for receipt in &self.receipts {
            receipt.0.store(FLUSHED, Ordering::SeqCst);
        }
        Ok(report)
    }

    /// Refuse a handle of another state.
    fn of(&self, shared: &Arc<Shared>) -> Result<()> {
        match Arc::ptr_eq(&self.shared, shared) {
            true => Ok(()),
            false => Err("the mutation is of another state".into()),
        }
    }
}

impl Drop for Mutation {
    fn drop(&mut self) {
        if !self.taken {
            for receipt in &self.receipts {
                receipt.0.store(DISCARDED, Ordering::SeqCst);
            }
        }
    }
}

/// What became of a [`Mutation`], for whoever moved changes into it
/// ([`Mutation::receipt`]): nothing yet, flushed (the changes are durable), or
/// discarded (the mutation was dropped, and the changes with it). A flush whose
/// outcome is unknown settles neither, and ends the state.
#[derive(Clone)]
pub struct Receipt(Arc<AtomicU8>);

const PENDING: u8 = 0;
const FLUSHED: u8 = 1;
const DISCARDED: u8 = 2;

impl Receipt {
    /// The state flushed the mutation.
    pub fn is_flushed(&self) -> bool {
        self.0.load(Ordering::SeqCst) == FLUSHED
    }

    /// The mutation was dropped without a flush.
    pub fn is_discarded(&self) -> bool {
        self.0.load(Ordering::SeqCst) == DISCARDED
    }
}

/// A named whole value the root keeps, replaced whole: the host's state
/// beside its files (a run's progress, an outbox), flushed with them in the
/// same root. Read from the state ([`Self::get`]), set in a mutation
/// ([`Mutation::set`]). A cheap handle.
///
/// A value of up to [`StateOptions::inline_record_bytes`] is in the root,
/// which every flush rewrites; a larger one is a blob of its own, written
/// when it changes. Either way, opening the state reads it.
#[derive(Clone)]
pub struct Record {
    name: Arc<str>,
    shared: Arc<Shared>,
}

impl Record {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value as the state holds it: as last flushed; empty if never set.
    pub fn get(&self) -> Bytes {
        let snapshot = self.shared.snapshot();
        snapshot
            .records()
            .get(&*self.name)
            .cloned()
            .unwrap_or_default()
    }
}

/// A named log: entries appended in a mutation ([`Mutation::append`]), each
/// at the next index, and dropped from its start ([`Mutation::truncate`]),
/// read from the state ([`Self::entries`]). A cheap handle.
///
/// Each entry is a blob of its own, written once (the root, which every
/// flush rewrites, holds only its number); opening the state reads every
/// entry. A flush costs about what it appends: what a log is for is changes
/// too small to rewrite their whole, folded into a record or a file now and
/// then, and truncated. A value of a few bytes is better in a record.
#[derive(Clone)]
pub struct Log {
    name: Arc<str>,
    shared: Arc<Shared>,
}

impl Log {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The index of the first entry the log holds, as last flushed: of the
    /// next entry if it holds none.
    pub fn first(&self) -> u64 {
        self.bounds().0
    }

    /// The index the next entry appended takes, as last flushed.
    pub fn next(&self) -> u64 {
        self.bounds().1
    }

    /// The entries the log holds, as last flushed, from [`Self::first`].
    pub fn entries(&self) -> Vec<Bytes> {
        self.shared.snapshot().log(&self.name).values
    }

    fn bounds(&self) -> (u64, u64) {
        self.shared.snapshot().log_bounds(&self.name)
    }
}

/// A paged file: 4 KiB pages, read at random from the state and written at
/// random in a mutation, made durable by its flush with its other files and its
/// records. An absent page, never written or past the end, reads as `None`
/// (zeros). A cheap handle.
///
/// Stored as chunks of [`StateOptions::chunk_pages`] pages, each a base and
/// maybe a delta against it, which the [`PageCache`] keeps across states.
///
/// Two handles are equal (and hash alike) when they name the same file of
/// the same state.
#[derive(Clone)]
pub struct File {
    name: Arc<str>,
    shared: Arc<Shared>,
}

impl PartialEq for File {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared) && self.name == other.name
    }
}

impl Eq for File {}

impl std::hash::Hash for File {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.shared).hash(state);
        self.name.hash(state);
    }
}

impl File {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bytes in the file, as last flushed.
    pub fn len(&self) -> u64 {
        self.shared.snapshot().file_size(&self.name)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Page `page` as the state holds it; `None` for zeros (a hole of a file
    /// without a base, or past its end).
    pub async fn read(&self, page: u64) -> Result<Option<Page>> {
        let snapshot = self.shared.snapshot();
        if page.saturating_mul(PAGE) >= snapshot.file_size(&self.name) {
            return Ok(None);
        }
        Ok(snapshot.read(&self.name, page).await?)
    }
}

/// Refuse a name no flush could store.
fn check_name(name: &str) -> Result<()> {
    match name.len() > MAX_NAME_BYTES {
        true => Err(format!("name {name:?} exceeds {MAX_NAME_BYTES} bytes").into()),
        false => Ok(()),
    }
}

/// Bytes of a page, as the `u64` that offsets and sizes are.
const PAGE: u64 = PAGE_SIZE as u64;

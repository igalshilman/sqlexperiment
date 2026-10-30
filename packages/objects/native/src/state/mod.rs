//! Transactional state over an asynchronous key/value store: paged files
//! and records, changed through a mutation in memory, which a flush
//! makes durable at once.
//!
//! A [`State`] is one namespace in a KV store: the state of a Restate object
//! without Restate's journal, which keeps writes in issue order and makes
//! them durable at a sync (see [`kv`]). It needs no multi-key transaction or
//! compare-and-swap; only collecting garbage lists keys. Its [`File`]s are 4 KiB pages read
//! and written at random, its [`Record`]s whole values. The state is
//! read; it changes only through a [`Mutation`] ([`State::mutation`]): the
//! records set and pages written there become durable
//! together at [`Mutation::flush`], and a mutation dropped discards them.
//!
//! The state knows nothing of what writes it: the `sql` crate keeps its
//! database in the file `db`, moving what it committed into a mutation
//! before the host flushes it, and the host sets records of its own
//! beside it. Coordinating them is theirs.
//!
//! # A tail and checkpoints
//!
//! Where the state is stored is split by what each store is good at (see
//! the private `format` and `store` modules, and `docs/ARCHITECTURE.md`). A flush
//! puts its mutation as one entry of a *tail* in Restate, then the *head*,
//! and syncs once: one round trip, which Restate fences. Now and then
//! ([`StateOptions::checkpoint_entries`]), a flush first writes the state as
//! a *checkpoint*: the frames of pages the tail changed, and the values it
//! set, into one *pack*, and a manifest of where every frame and value is,
//! in Restate. Its head then names the checkpoint, and drops the tail it
//! covered. Packs are tiered: a checkpoint's pack goes to *L0*, in Restate's
//! state, until L0 holds [`StateOptions::l0_bytes`]; then one goes to *L1*,
//! a [`blobs::BlobStore`] (S3, through [`Open::checkpoints`]), with
//! everything L0 held. Without an L1, every pack stays in Restate. Pack
//! names are unique, and packs never change, so S3 needs no fencing, nor
//! conditional puts.
//!
//! | Layer | Representation | Survives? |
//! |---|---|---|
//! | A mutation | Records set and pages written in it, in memory | No: dropping it discards them |
//! | Head | `v22/head`: the version, the checkpoint and the one before, the tail's extent | Yes, after an acknowledged flush |
//! | Tail | `v22/tail/<version>/<part>`: the mutation a flush made: records, pages (each a zstd frame against the page before it) | Yes, until a checkpoint after the next one |
//! | Manifests | `v22/manifest/<version>-<nonce>/<part>`: where a checkpoint's frames and values are | Yes, while the head names its checkpoint |
//! | Packs | `l0/<version>-<nonce>` in Restate, `l1/<version>-<nonce>` in the blob store: frames of [`StateOptions::frame_pages`] pages, large values | Yes, until no checkpoint the head names needs it |
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including
//! settlement of cancelled requests before reopening or handing ownership to
//! another session. There is no distributed lock or fence here.
//!
//! Any number of states may read beside the owner ([`Open::reader`], or
//! states of the owner that overlap in a [`PageCache`]): each reads the
//! state it opened, a consistent snapshot, while the owner flushes, because
//! packs never change, and the tail and the packs a head names stay for one
//! checkpoint more. Only one flushes: a flush is refused if another state
//! of the owner flushed since this one opened.
//!
//! The [`PageCache`] is the process's: it keeps ranges of packs in memory
//! and on a local disk tier under one budget for every namespace it opens,
//! under their location, for as long as it likes: a pack never changes. It
//! remembers the head this owner made durable, and the state as of it, so a
//! steady-state open and flush are each one round trip. A value may be parked there for the next
//! state of the namespace ([`State::park`]), such as an SQL engine, which
//! takes it back when nothing changed. The disk tier's I/O runs on tokio.
//!
//! # Example
//!
//! ```ignore
//! # use std::sync::Arc;
//! # use crate::state::{State, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> crate::state::Result<()> {
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
//! [`MAX_RECORD_BYTES`], a name
//! [`MAX_NAME_BYTES`]. The backend is assumed
//! to return what was stored; decoding checks structure and bounds, and
//! zstd frames carry a checksum. Use a fresh, dedicated namespace; no
//! migration or format discovery is implemented. The simulated stores
//! ([`simulation`]) are in tests, and with the crate's `bench` feature.
pub mod blobs;
mod cache;
mod format;
pub mod kv;
#[cfg(any(test, feature = "bench"))]
pub mod simulation;
mod store;
mod tail;
mod zstd;

pub use cache::{PageCache, PageCacheConfig};
pub use format::{HEAD_KEY, MAX_FILE_BYTES, MAX_NAME_BYTES, MAX_RECORD_BYTES, TAIL_PREFIX};
use store::Is;
pub use store::{Checkpointed, FlushReport};

use crate::state::blobs::BlobStore;
use crate::state::cache::{Backoff, NamespaceCache, ParkedValue};
use crate::state::kv::KvStore;
use crate::state::store::Snapshot;
use crate::state::tail::Tiered;
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

/// Default [`StateOptions::frame_pages`].
pub const FRAME_PAGES: u64 = 4;

/// How a state flushes and reads. Only [`Self::frame_pages`] is stored, when
/// the namespace is made; the others never change the stored format.
#[derive(Clone)]
pub struct StateOptions {
    /// Pages per frame of a checkpoint, the unit a read fetches and a
    /// checkpoint rewrites: larger frames compress better and make a
    /// smaller manifest, smaller ones cost a checkpoint less to rewrite. Set
    /// when the namespace is made; its head keeps it.
    pub frame_pages: u64,
    /// Bytes of a record a checkpoint's manifest holds; a
    /// larger one is a value of its own in the pack.
    pub inline_record_bytes: usize,
    /// Every frame and patch of a file of at most this many
    /// pages, is fetched at open, alongside the manifest's values.
    pub prefetch_pages: u64,
    /// A flush checkpoints first ([`Mutation::flush`]) once the tail holds
    /// this many entries since the last checkpoint; 0 never does by count.
    pub checkpoint_entries: u64,
    /// ... or this many bytes; 0 never does by size. A checkpoint into L0
    /// (Restate) is due at most at the bytes L0 and the manifest hold, but at
    /// least a frame's: it costs only its bytes, and spares the tail's,
    /// though a round trip or two too. One into L1 (S3) is due at exactly
    /// these.
    pub checkpoint_bytes: u64,
    /// With packs in S3 ([`Open::checkpoints`]), a checkpoint's pack goes to
    /// L0, in Restate, until the packs there hold this many bytes; the
    /// checkpoint after that puts its pack in S3, and moves everything L0
    /// holds into it. 0 puts every checkpoint in S3; without S3, every
    /// pack stays in Restate.
    pub l0_bytes: u64,
}

impl Default for StateOptions {
    fn default() -> Self {
        Self {
            frame_pages: FRAME_PAGES,
            inline_record_bytes: 1024,
            prefetch_pages: 16,
            checkpoint_entries: 128,
            checkpoint_bytes: 1 << 20,
            l0_bytes: 4 << 20,
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
    /// Where the state is stored: the head, the tail, the packs.
    tail: Arc<Tiered>,
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
    /// When the next checkpoint may be tried, after some failed: the
    /// cache's, which the namespace's next states share, if it has one.
    backoff: Mutex<Backoff>,
}

impl Shared {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Change the backoff with `f`, here and in the cache.
    fn backoff(&self, f: impl FnOnce(&mut Backoff)) {
        let mut backoff = self.backoff.lock().unwrap();
        f(&mut backoff);
        if let Some(cache) = &self.cache {
            cache.set_backoff(*backoff);
        }
    }

    /// Delete the packs no checkpoint the head names needs, and the
    /// manifests of checkpoints older than the previous one. A failure, or
    /// dropping the future midway, ends the state: in Restate, what it
    /// deleted has an unknown outcome, which the next open syncs. With
    /// `all`, S3 is listed whatever the head; without, only where L1's
    /// packs may have changed ([`Tiered::collect_garbage`]).
    async fn collect_garbage(&self, all: bool) -> Result<usize> {
        let mut collecting = Collecting {
            shared: self,
            done: false,
        };
        let head = self.snapshot().head().clone();
        let deleted = self.tail.collect_garbage(&head, all).await?;
        collecting.done = true;
        Ok(deleted)
    }
}

/// A running garbage collection: see [`Shared::collect_garbage`].
struct Collecting<'a> {
    shared: &'a Shared,
    done: bool,
}

impl Drop for Collecting<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.shared.poisoned.store(true, Ordering::SeqCst);
        if let Some(cache) =
            (self.shared.cache.as_ref()).filter(|_| !self.shared.reader.load(Ordering::SeqCst))
        {
            cache.forget();
        }
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

impl Flushing<'_> {
    /// Refuse the flush before it wrote anything: the state goes on.
    fn refuse<T>(&mut self, error: Box<dyn std::error::Error + Send + Sync>) -> Result<T> {
        (self.returned, self.done) = (true, true);
        Err(error)
    }
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
    /// Open the namespace over `store`, Restate's state: read its head, its
    /// checkpoint's manifest and its tail (and its large records and small
    /// files) before anything reads; a crash leaves nothing to recover. Await it as it is, or set
    /// how first:
    ///
    /// ```ignore
    /// # use std::sync::Arc;
    /// # use crate::state::{PageCache, PageCacheConfig, State, StateOptions, kv::KvStore};
    /// # async fn example(store: Arc<dyn KvStore>, cache: PageCache) -> crate::state::Result<()> {
    /// let state = State::open(store.clone()).await?;
    /// let state = State::open(store.clone()).cached(&cache, "object-42").await?;
    /// let state = State::open(store)
    ///     .options(StateOptions { checkpoint_entries: 64, ..StateOptions::default() })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(store: Arc<dyn KvStore>) -> Open {
        Open {
            store,
            packs: None,
            options: StateOptions::default(),
            cache: None,
            reader: false,
        }
    }

    async fn open_as(open: Open) -> Result<Self> {
        let Open {
            store,
            packs,
            options,
            cache,
            reader,
        } = open;
        let tail = Arc::new(Tiered::new(store, packs));
        let snapshot = Snapshot::open(tail.clone(), options.clone(), cache.clone(), reader);
        let snapshot = snapshot.await?;
        let mut parked = match &cache {
            Some(cache) if snapshot.unchanged => cache.take_parked(),
            _ => HashMap::new(),
        };
        parked.retain(|_, value| value.version == snapshot.version());
        let backoff = cache
            .as_ref()
            .map(NamespaceCache::backoff)
            .unwrap_or_default();
        Ok(Self {
            shared: Arc::new(Shared {
                snapshot: Mutex::new(Arc::new(snapshot)),
                tail,
                options,
                cache,
                parked: Mutex::new(parked),
                reader: AtomicBool::new(reader),
                poisoned: AtomicBool::new(false),
                flushing: AtomicBool::new(false),
                backoff: Mutex::new(backoff),
            }),
            prefix: Arc::from(""),
        })
    }

    /// The options this state opened with.
    pub fn options(&self) -> &StateOptions {
        &self.shared.options
    }

    /// The record named `name`: a whole value, read here and set in a
    /// mutation (see [`Record`]).
    pub fn record(&self, name: &str) -> Record {
        Record {
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

    /// The part of the state whose names start with `prefix`, named without
    /// it (`scoped("fs/")` makes `manifest` the record `fs/manifest`).
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
    /// would keep it open meanwhile, on a state whose packs later flushes
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
            receipts: Vec::new(),
            checkpoint: false,
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

    /// Delete the packs no checkpoint the head names needs (those older
    /// than the checkpoint before the head's, which neither names), and the
    /// manifests of checkpoints older than that one. How
    /// many. A flush that makes a checkpoint collects too.
    ///
    /// Any state may collect, an owner's or a reader's, beside the owner's
    /// flushes, or one Restate fenced: whatever head it read, a pack a later
    /// head needs is one its head names, or newer than its previous
    /// checkpoint. A failure, or dropping the future midway, ends the state,
    /// as a failed flush does.
    pub async fn collect_garbage(&self) -> Result<usize> {
        if self.shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        self.shared.collect_garbage(true).await
    }

    /// The packs the head's checkpoints need: those its checkpoint and the
    /// one before name. Others are garbage.
    #[cfg(any(test, feature = "bench"))]
    pub async fn needed_packs(&self) -> Result<std::collections::BTreeSet<String>> {
        let head = self.shared.snapshot().head().clone();
        let needed = self.shared.tail.needed(&head).await?;
        Ok(needed.iter().map(|pack| pack.key()).collect())
    }

    /// The manifests the head names, by name: its checkpoint's and the one
    /// before's. Others are garbage.
    #[cfg(any(test, feature = "bench"))]
    pub fn needed_manifests(&self) -> std::collections::BTreeSet<String> {
        let head = self.shared.snapshot().head().clone();
        (head.checkpoint.iter().chain(&head.previous))
            .map(|checkpoint| checkpoint.pack.name())
            .collect()
    }
}

/// How to open a [`State`] ([`State::open`]): await it to open.
#[must_use = "a state opens when awaited"]
pub struct Open {
    store: Arc<dyn KvStore>,
    packs: Option<Arc<dyn BlobStore>>,
    options: StateOptions,
    cache: Option<NamespaceCache>,
    reader: bool,
}

impl Open {
    /// Flush and read with `options` rather than the defaults.
    pub fn options(self, options: StateOptions) -> Self {
        Self { options, ..self }
    }

    /// Keep the bulk of the checkpoints in `packs`, S3 say
    /// ([`blobs::ObjectStoreBlobs`]), as L1: the latest few go to Restate
    /// first ([`StateOptions::l0_bytes`]). Every state of the namespace must.
    pub fn checkpoints(self, packs: Arc<dyn BlobStore>) -> Self {
        Self {
            packs: Some(packs),
            ..self
        }
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
    /// state whose mutations refuse to flush (it may still collect garbage,
    /// [`State::collect_garbage`]). It reads the state as of open, whatever the owner flushes meanwhile,
    /// until the owner's second checkpoint after it. With a cache
    /// ([`Self::cached`]), it reads the ranges of packs the owner cached.
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
        Box::pin(State::open_as(self))
    }
}

/// A change to a [`State`]: the records set and pages written since
/// [`State::mutation`] made it, in memory, which
/// [`Self::flush`] makes durable together, atomically. There is no other way to change the state. A mutation
/// dropped discards its writes; whoever moved changes into it learns which
/// happened from its [`Receipt`].
pub struct Mutation {
    shared: Arc<Shared>,
    records: store::Records,
    files: store::Writes,
    receipts: Vec<Receipt>,
    /// [`Self::checkpoint`]: checkpoint first, however short the tail.
    checkpoint: bool,
    /// [`Self::flush`] took its writes: dropping discards nothing.
    taken: bool,
}

impl Mutation {
    /// Whether nothing was set or written.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.files.is_empty()
    }

    /// Have [`Self::flush`] checkpoint first, however short the tail, if it
    /// writes anything: the checkpoint holds every flush before this one.
    pub fn checkpoint(&mut self) {
        self.checkpoint = true;
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
    /// regrowing it reads zeros. Bytes of a last,
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

    /// Refuse `name` as `wanted` if it is another kind, in the state or in
    /// this mutation, or a name no flush could store.
    fn claim(&self, name: &str, wanted: Is) -> Result<()> {
        check_name(name)?;
        let mine = (self.records.contains_key(name).then_some(Is::Record))
            .or(self.files.contains_key(name).then_some(Is::Paged));
        match mine.or(self.shared.snapshot().is(name)) {
            Some(held) if held != wanted => Err(held.refuse(name, wanted).into()),
            _ => Ok(()),
        }
    }

    /// Make every record set and page written durable: as
    /// a tail entry, then the head, then one sync, in one round trip. Nothing changed: nothing is written. It is refused while another flush of the state
    /// runs, and for writes the state cannot store; a refused flush writes
    /// nothing, and the state goes on. `Ok`
    /// holds what it wrote; any other error (one refused because another
    /// state flushed since included), or dropping the future, ends the
    /// state: open it again.
    ///
    /// Once the tail passes [`StateOptions::checkpoint_entries`] or
    /// [`StateOptions::checkpoint_bytes`], a flush first checkpoints the
    /// durable state: it puts a pack, which its head names; then it
    /// collects garbage ([`State::collect_garbage`]). A checkpoint that
    /// fails changes nothing: the flush goes on, and a later one
    /// checkpoints. A collection that fails ends the state, though the
    /// flush landed: its receipts say so.
    pub async fn flush(mut self) -> Result<FlushReport> {
        let shared = self.shared.clone();
        if shared.reader.load(Ordering::SeqCst) {
            return Err("a reader writes nothing".into());
        }
        if shared.flushing.swap(true, Ordering::SeqCst) {
            return Err("a flush of this state is running".into());
        }
        let mut running = Flushing {
            shared: &shared,
            returned: false,
            done: false,
        };
        // Read once no other flush runs: one that ended before this one
        // started is in the snapshot, so this one builds on it.
        if shared.poisoned.load(Ordering::SeqCst) {
            return running.refuse(ENDED.into());
        }
        let snapshot = shared.snapshot();
        // What the writes checked as they were made still holds, unless
        // another mutation flushed since: refused before anything is written.
        if let Err(error) = (snapshot).check(&self.files, &self.records).await {
            return running.refuse(error.into());
        }
        let mut files = std::mem::take(&mut self.files);
        let mut records = std::mem::take(&mut self.records);
        self.taken = true;
        // Only a flush that writes a head adopts a checkpoint. A failed
        // checkpoint fails nothing: the report says why, and a later flush
        // tries again, the next one first, then fewer and fewer of them
        // ([`Backoff`]); one asked for is tried regardless.
        let version = snapshot.version();
        let allowed = shared.backoff.lock().unwrap().allows(version);
        let due = self.checkpoint || (snapshot.due() && allowed);
        let (staged, failed) = match snapshot.changes(&mut files, &mut records) && due {
            true => match snapshot.checkpoint().await {
                Ok(staged) => (Some(staged), None),
                Err(error) => {
                    shared.backoff(|backoff| backoff.failed(version));
                    (None, Some(error.to_string()))
                }
            },
            false => (None, None),
        };
        let result = snapshot.commit(files, records, staged).await;
        running.returned = true;
        let (mut report, after) = result?;
        if report.checkpoint.is_some() {
            shared.backoff(Backoff::succeeded);
        }
        report.checkpoint_failed = failed;
        *shared.snapshot.lock().unwrap() = Arc::new(after);
        running.done = true;
        for receipt in &self.receipts {
            receipt.0.store(FLUSHED, Ordering::SeqCst);
        }
        if let Some(checkpoint) = &mut report.checkpoint {
            checkpoint.deleted = shared.collect_garbage(false).await?;
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

/// A named whole value, replaced whole: the host's state beside its files
/// (a request's outbox, say), flushed with them. Read from the state
/// ([`Self::get`]), set in a mutation ([`Mutation::set`]). A cheap handle.
///
/// A flush puts the value in its tail entry; a checkpoint keeps one of up
/// to [`StateOptions::inline_record_bytes`] in its manifest, and a larger
/// one as a value of its own. Opening the state reads it.
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

/// A paged file: 4 KiB pages, read at random from the state and written at
/// random in a mutation, made durable by its flush with its other files and its
/// records. An absent page, never written or past the end, reads as `None`
/// (zeros). A cheap handle.
///
/// A page flushed is in the tail, a zstd frame of its own, until a
/// checkpoint writes it into a frame of [`StateOptions::frame_pages`]
/// pages, which a read fetches whole and the [`PageCache`] keeps across
/// states.
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

    /// Page `page` as the state holds it; `None` for zeros (a hole, or past
    /// its end).
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

#[cfg(test)]
mod tests;

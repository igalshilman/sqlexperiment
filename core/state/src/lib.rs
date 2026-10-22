//! Transactional state over an asynchronous key/value store: paged files and
//! records, changed through a write-ahead log, which a flush makes durable
//! in one put.
//!
//! A [`State`] is one namespace in a KV store: the state of a Restate object
//! without Restate's journal, which keeps writes in issue order and makes
//! them durable at a sync (see [`kv`]). It needs no multi-key transaction,
//! compare-and-swap, listing operation or backend WAL API. Its [`File`]s are
//! 4 KiB pages read and written at random, and its [`Record`]s whole values
//! in the commit record. The state is read; it changes only through a
//! [`Wal`] ([`State::wal`]): the records set and pages written there become
//! durable together at [`Wal::flush`], and a WAL dropped discards them.
//!
//! What writes a WAL is a [`Participant`]: it keeps changes of its own, moves
//! them into the WAL before the flush, and may leave state idle for the
//! namespace's next [`State`]. The `sql` crate's database is one, on the
//! file `db`.
//!
//! # When changes become durable
//!
//! At [`Wal::flush`], and only then: one put makes every record set and page
//! written in the WAL durable together. The host decides when to flush: after
//! each SQL transaction (as SQLite does), or once per turn of everything that
//! took part.
//!
//! | Layer | Representation | Survives? |
//! |---|---|---|
//! | A WAL | Records set and pages written in it, in memory | No: dropping it discards them |
//! | Commit record | `commit`: the log since the last checkpoint, one transaction per flush, compressed against the previous images; a large one's segments at `log/<n>` | Yes, after an acknowledged flush |
//! | Pages | `page/<file>/<n>`, overwritten in place by checkpoints | Yes |
//!
//! A flush puts the record with one more transaction, usually a single put,
//! then one sync. A WAL that changes nothing writes nothing. An error or
//! cancellation can have an unknown outcome. Settle outstanding backend
//! requests and reopen before deciding whether to retry application work.
//!
//! # Why a log and pages in place?
//!
//! One put is the only atomic operation the backend offers: a crash may keep
//! some of a flush's writes and lose the ones after them. The record turns one
//! put into a multi-page flush: a transaction is visible once a record
//! holding it is. Because the log holds every page a checkpoint has not yet
//! recorded, checkpoints may overwrite `page/<n>` in place, after the record:
//! replay applies the log over whatever they left. This is SQLite's own WAL
//! design, applied to KV keys. There is no page map, no object allocation, no
//! intent and no orphan collection. See `docs/DESIGN.md` and the private
//! `snapshot` and `log` modules.
//!
//! The costs are writing each page twice (log, then checkpoint) and replaying
//! the log on open. A logged page costs about what changed in it since its
//! previous image, so checkpoints can be rare. This is not MVCC: one owner
//! reads and writes the namespace.
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including
//! settlement of cancelled requests before reopening or handing ownership to
//! another session. There is no distributed lock or fence here. A flush
//! queues its record, then, when the log is long, its checkpoint, and syncs
//! once. Open reads the record, and the stored segments its log names, if
//! any.
//!
//! States of one owner may overlap when they share a [`NamespaceCache`]:
//! each reads the state it opened at (a checkpoint waits while another state
//! holds an older version), and a flush is refused if another state flushed
//! since this one opened. Without a cache they must not overlap.
//!
//! [`StorageOptions::cache`] can hold a [`NamespaceCache`]: one namespace's
//! handle on the process-wide [`PageCache`], which keeps pages in memory and
//! on a local disk tier under one budget for every namespace. It keeps them
//! while the record's epoch (its checkpoint count) is the one they belong to,
//! and remembers the record this owner made durable, so a steady-state open
//! and flush are each one round trip. A participant may leave idle state
//! there ([`Participant::park`]); the next state hands it back when nothing
//! changed. The disk tier's I/O runs on tokio.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use state::{State, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> state::Result<()> {
//! let state = State::open(store).await?;
//! let mut wal = state.wal();
//! state.record("progress").set(&mut wal, "step 1".into())?;
//! state.file("image")?.write(&mut wal, 0, Some(vec![7; 4096].into()))?;
//! wal.flush().await?; // the record and the page, durable together
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! Log segments are capped at [`MAX_SEGMENT_BYTES`]; larger flushes span
//! several. A record's value is capped at [`MAX_RECORD_BYTES`], its name at
//! [`MAX_NAME_BYTES`]. The backend is assumed to return what was stored;
//! decoding checks structure and bounds, not checksums. Use a fresh,
//! dedicated namespace; no migration or format discovery is implemented.
//! Simulation and benchmark support require the opt-in `simulation` feature.
mod cache;
mod format;
mod hint;
pub mod kv;
mod log;
pub mod page_codec;
#[cfg(feature = "simulation")]
pub mod simulation;
mod snapshot;

pub use cache::{NamespaceCache, PageCache, PageCacheConfig};
pub use hint::MAX_HINT_PAGES;
pub use log::{Base, MAX_NAME_BYTES, MAX_RECORD_BYTES, MAX_SEGMENT_BYTES};
pub use page_codec::{Page, PageCompression};
pub use snapshot::{
    CHECKPOINT_AFTER, CHECKPOINT_LOG_BYTES, FlushReport, INLINE_LOG_BYTES, StorageOptions,
};

use crate::cache::Idle;
use crate::kv::KvStore;
use crate::snapshot::Snapshot;
use bytes::Bytes;
use std::{
    any::Any,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

/// State and backend errors returned to the host task.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Bytes of a page of a [`File`].
pub const PAGE_SIZE: usize = 4096;

/// One namespace's state: its paged files and its records, as last flushed.
/// It is read here, and changed through a [`Wal`] ([`Self::wal`]), which
/// [`Wal::flush`] makes durable in one put. A cheap handle; clones, and the
/// views of [`Self::scoped`], share it.
///
/// Acquire exclusive ownership of the namespace and settle the preceding
/// owner's requests before opening it; a reader ([`Self::open_reader`]) may
/// open beside the owner.
#[derive(Clone)]
pub struct State {
    shared: Arc<Shared>,
    /// Prefixed to every name: a view of part of the state.
    prefix: Arc<str>,
}

/// What a state's handles share.
struct Shared {
    /// The durable state: as opened, then as the WALs flushed it.
    snapshot: Mutex<Arc<Snapshot>>,
    options: StorageOptions,
    /// The participants that joined, by name: whether each holds changes not
    /// yet in a WAL.
    participants: Mutex<HashMap<String, bool>>,
    /// What participants left idle at the state this one opened on, by
    /// participant, until they take it.
    idle: Mutex<HashMap<String, Idle>>,
    /// A reader's ([`State::open_reader`]): it never flushes.
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

    /// A participant holding changes not yet in a WAL.
    fn unflushed(&self) -> Option<String> {
        let joined = self.participants.lock().unwrap();
        let mut unflushed = joined.iter().filter(|(_, unflushed)| **unflushed);
        unflushed.next().map(|(name, _)| name.clone())
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
            && let Some(cache) = &self.shared.options.cache
        {
            cache.forget();
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Idle state no participant took goes back, if nothing changed.
        let Some(cache) = &self.options.cache else {
            return;
        };
        let version = self.snapshot().version();
        for (name, idle) in self.idle.get_mut().unwrap().drain() {
            if idle.version == version && cache.committed() == Some(version) {
                cache.park(&name, idle);
            }
        }
    }
}

/// The error of a state that a failed flush ended.
const ENDED: &str = "the state ended after a failed or cancelled flush; open a new one";

impl State {
    /// Open the namespace with default [`StorageOptions`]: read its commit
    /// record (and the stored values it names), recovering what a crash
    /// left, before anything reads.
    pub async fn open(store: Arc<dyn KvStore>) -> Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }

    /// Open with explicit [`StorageOptions`]. Options never change the stored
    /// format: existing page tags determine decoding. With a [`PageCache`],
    /// what participants of this owner's previous state left idle waits for
    /// them ([`Participant::take_idle`]) when nothing changed since.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> Result<Self> {
        Self::open_as(store, options, false).await
    }

    /// Open a reader of a namespace that another owns, alongside it: a state
    /// that writes nothing to the store (its WALs refuse to flush) and keeps
    /// no cache. It reads the state as of open, unless the owner checkpoints
    /// meanwhile: [`Self::is_current`], once it has read, says whether it
    /// did.
    pub async fn open_reader(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<Self> {
        let options = StorageOptions {
            cache: None,
            ..options
        };
        Self::open_as(store, options, true).await
    }

    async fn open_as(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
        reader: bool,
    ) -> Result<Self> {
        let snapshot = Snapshot::open(store, options.clone(), reader).await?;
        let mut idle = match &options.cache {
            Some(cache) if snapshot.unchanged => cache.take_idle(),
            _ => HashMap::new(),
        };
        idle.retain(|_, idle| idle.version == snapshot.version());
        Ok(Self {
            shared: Arc::new(Shared {
                snapshot: Mutex::new(Arc::new(snapshot)),
                options,
                participants: Mutex::default(),
                idle: Mutex::new(idle),
                reader: AtomicBool::new(reader),
                poisoned: AtomicBool::new(false),
                flushing: AtomicBool::new(false),
            }),
            prefix: Arc::from(""),
        })
    }

    /// The options this state opened with.
    pub fn options(&self) -> &StorageOptions {
        &self.shared.options
    }

    /// The record named `name`: a whole value the commit record holds, read
    /// here and set in a WAL (see [`Record`]).
    pub fn record(&self, name: &str) -> Record {
        Record {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        }
    }

    /// The paged file named `name`: 4 KiB pages, read here and written in a
    /// WAL (see [`File`]).
    pub fn file(&self, name: &str) -> Result<File> {
        Ok(File {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        })
    }

    /// The paged file named `name`, over `base` (see [`Base`]): an absent
    /// page, or a hole, reads as the base's page, and pages are compressed
    /// against it. A file made with a base opens only with that one again
    /// (by [`Base::id`]); its log since the last checkpoint decodes now.
    /// [`Self::file`] opens it without its base, to write holes and set its
    /// length only: emptying it for another base, say.
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

    /// Take part in the state as `name`, one at a time per name (see
    /// [`Participant`]).
    pub fn join(&self, name: &str) -> Result<Participant> {
        let name = format!("{}{name}", self.prefix);
        let mut joined = self.shared.participants.lock().unwrap();
        if joined.contains_key(&name) {
            return Err(format!("{name:?} already takes part in this state").into());
        }
        joined.insert(name.clone(), false);
        Ok(Participant {
            name: name.into(),
            state: self.clone(),
        })
    }

    /// A new, empty WAL on this state: the records set and pages written in
    /// it ([`Record::set`], [`File::write`]) become durable together at
    /// [`Wal::flush`]; dropping it discards them.
    pub fn wal(&self) -> Wal {
        Wal {
            shared: self.shared.clone(),
            records: Default::default(),
            files: Default::default(),
            receipts: Vec::new(),
            taken: false,
        }
    }

    /// The namespace's version: the flushes ever made, as this state holds
    /// it (opened on, or flushed itself).
    pub fn version(&self) -> u64 {
        self.shared.snapshot().version()
    }

    /// Whether the store still holds the state this one opened on (see
    /// [`Self::open_reader`]): if it does not, what was read may mix that
    /// state with a later checkpoint's pages, so read again in a new one. A
    /// state's own flushes change it, so this is for one that has not
    /// flushed.
    pub async fn is_current(&self) -> Result<bool> {
        Ok(self.shared.snapshot().is_current().await?)
    }

    /// Whether the state can flush: a failed or cancelled flush ends it.
    pub fn is_usable(&self) -> bool {
        !self.shared.poisoned.load(Ordering::SeqCst)
    }
}

/// The writes ahead of a [`State`]: the records set and pages written since
/// [`State::wal`] made it, in memory, which [`Self::flush`] makes durable
/// together, in one put. There is no other way to change the state. A WAL
/// dropped discards its writes; a participant that moved changes into it
/// learns which happened from its [`Receipt`].
pub struct Wal {
    shared: Arc<Shared>,
    records: log::Records,
    files: snapshot::Writes,
    receipts: Vec<Receipt>,
    /// [`Self::flush`] took its writes: dropping discards nothing.
    taken: bool,
}

impl Wal {
    /// Whether nothing was set or written.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.files.is_empty()
    }

    /// A receipt for what becomes of this WAL (see [`Receipt`]).
    pub fn receipt(&mut self) -> Receipt {
        let receipt = Receipt(Arc::new(AtomicU8::new(PENDING)));
        self.receipts.push(receipt.clone());
        receipt
    }

    /// Make every record set and page written durable, in one put (and a
    /// checkpoint, when the log is long), then one sync. Nothing changed:
    /// nothing is written. It is refused while a participant holds changes
    /// not yet in a WAL, and while another flush of the state runs; a
    /// refused flush writes nothing, and the state goes on. `Ok` holds what
    /// it wrote; any other error, or dropping the future, leaves the outcome
    /// unknown and ends the state: open it again.
    pub async fn flush(mut self) -> Result<FlushReport> {
        let shared = self.shared.clone();
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if shared.reader.load(Ordering::SeqCst) {
            return Err("a reader writes nothing".into());
        }
        if let Some(name) = shared.unflushed() {
            return Err(format!("{name:?} has changes not yet in a WAL: flush it first").into());
        }
        let snapshot = shared.snapshot();
        // What the writes checked as they were made still holds, unless
        // another WAL flushed since: refused before anything is written.
        snapshot.check(&self.files, &self.records)?;
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
        self.taken = true;
        let result = snapshot.commit(files, records).await;
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
            false => Err("the WAL is of another state".into()),
        }
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        if !self.taken {
            for receipt in &self.receipts {
                receipt.0.store(DISCARDED, Ordering::SeqCst);
            }
        }
    }
}

/// What became of a [`Wal`], for a participant that moved changes into it
/// ([`Wal::receipt`]): nothing yet, flushed (the changes are durable), or
/// discarded (the WAL was dropped, and the changes with it). A flush whose
/// outcome is unknown settles neither, and ends the state.
#[derive(Clone)]
pub struct Receipt(Arc<AtomicU8>);

const PENDING: u8 = 0;
const FLUSHED: u8 = 1;
const DISCARDED: u8 = 2;

impl Receipt {
    /// The state flushed the WAL.
    pub fn is_flushed(&self) -> bool {
        self.0.load(Ordering::SeqCst) == FLUSHED
    }

    /// The WAL was dropped without a flush.
    pub fn is_discarded(&self) -> bool {
        self.0.load(Ordering::SeqCst) == DISCARDED
    }
}

/// Something that takes part in a state under a name ([`State::join`]): it
/// keeps changes of its own (an SQL engine's pages, say), and moves them
/// into a WAL before the flush. While it says it holds changes not yet
/// moved ([`Self::set_unflushed`]), every WAL of the state refuses to flush.
/// Dropping it leaves the name free, and its unmoved changes out.
///
/// With a [`PageCache`], it may leave state idle for the namespace's next
/// [`State`] ([`Self::park`]), which that state's participant of the same
/// name takes back ([`Self::take_idle`]) when nothing changed since.
pub struct Participant {
    name: Arc<str>,
    state: State,
}

impl Participant {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The state it takes part in.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Say whether it holds changes not yet in a WAL.
    pub fn set_unflushed(&self, unflushed: bool) {
        let mut joined = self.state.shared.participants.lock().unwrap();
        joined.insert(self.name.to_string(), unflushed);
    }

    /// What this participant left idle at the end of the owner's previous
    /// state, if nothing changed since and it is a `T`.
    pub fn take_idle<T: Any + Send>(&self) -> Option<T> {
        let idle = self
            .state
            .shared
            .idle
            .lock()
            .unwrap()
            .remove(&*self.name)?;
        idle.value.downcast().ok().map(|value| *value)
    }

    /// Whether [`Self::park`] would keep state now: the state has a
    /// [`PageCache`] and is at the owner's last durable flush.
    pub fn can_park(&self) -> bool {
        let state = &self.state;
        let Some(cache) = &state.shared.options.cache else {
            return false;
        };
        state.is_usable() && cache.committed() == Some(state.version())
    }

    /// Leave `value`, which corresponds to the state as last flushed, for
    /// the namespace's next state, weighing `weight` bytes against
    /// [`PageCacheConfig::idle_bytes`]. `Err` gives it back if it cannot be
    /// kept (see [`Self::can_park`]). The cache drops it when anything
    /// changes, or under its budget; one heavier than the whole budget is
    /// not kept. `value` must hold no handle of this state (a [`File`], a
    /// [`Record`], the [`State`]), which would keep it open meanwhile and
    /// hold back the namespace's checkpoints.
    pub fn park<T: Any + Send>(&self, value: T, weight: usize) -> std::result::Result<(), T> {
        if !self.can_park() {
            return Err(value);
        }
        let cache = self.state.shared.options.cache.as_ref().unwrap();
        let idle = Idle {
            value: Box::new(value),
            version: self.state.version(),
            weight,
        };
        cache.park(&self.name, idle);
        Ok(())
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        (self.state.shared.participants.lock().unwrap()).remove(&*self.name);
    }
}

/// A named whole value the commit record holds, replaced whole: the host's
/// state beside its files (a run's progress, an outbox), flushed with them
/// in the same put. Read from the state ([`Self::get`]), set in a WAL
/// ([`Self::set`]). A cheap handle.
///
/// A value of up to [`StorageOptions::inline_log_bytes`] is in the record,
/// which every flush rewrites; a larger one is a stored value of its own,
/// written when it changes. Either way, opening the state reads it.
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

    /// Replace the value with `wal`'s flush. Refused, before the WAL
    /// changes, is what no flush could store: a value past
    /// [`MAX_RECORD_BYTES`], a name past [`MAX_NAME_BYTES`], or the name of
    /// a paged file.
    pub fn set(&self, wal: &mut Wal, value: Bytes) -> Result<()> {
        wal.of(&self.shared)?;
        if value.len() > MAX_RECORD_BYTES {
            let (len, max) = (value.len(), MAX_RECORD_BYTES);
            return Err(format!("record {:?} of {len} bytes exceeds {max}", self.name).into());
        }
        check_name(&self.name)?;
        let paged = self.shared.snapshot().is_paged(&self.name);
        if wal.files.contains_key(&*self.name) || paged == Some(true) {
            return Err(format!("{:?} is a paged file, not a record", self.name).into());
        }
        wal.records.insert(self.name.to_string(), value);
        Ok(())
    }
}

/// A paged file: 4 KiB pages, read at random from the state and written at
/// random in a WAL, made durable by its flush with its other files and its
/// records. An absent page, never written or past the end, reads as `None`
/// (zeros). A cheap handle.
///
/// The commit record's log holds the changed pages until a checkpoint writes
/// them in place, and the [`PageCache`] keeps them across states.
#[derive(Clone)]
pub struct File {
    name: Arc<str>,
    shared: Arc<Shared>,
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

    /// Write `image` (4096 bytes) at `page`, or a hole with `None`, growing
    /// the file to include it, with `wal`'s flush. Refused, before the WAL
    /// changes, is what no flush could store: a page past the largest file,
    /// a name past [`MAX_NAME_BYTES`], or the name of a record.
    pub fn write(&self, wal: &mut Wal, page: u64, image: Option<Page>) -> Result<()> {
        if image.as_ref().is_some_and(|image| image.len() != PAGE_SIZE) {
            return Err("a page is 4096 bytes".into());
        }
        let end = page.saturating_add(1).saturating_mul(PAGE);
        if end > log::MAX_DATABASE_BYTES {
            return Err("a page past the largest file".into());
        }
        self.change(wal, |written| {
            written.size = written.size.max(end);
            written.pages.insert(page, image);
        })
    }

    /// Make the file `len` bytes, a multiple of 512, with `wal`'s flush:
    /// pages wholly past it become holes, so regrowing it reads zeros (or
    /// its base's pages). Bytes of a last, partial page past `len` stay as
    /// written.
    pub fn set_len(&self, wal: &mut Wal, len: u64) -> Result<()> {
        if !len.is_multiple_of(512) || len > log::MAX_DATABASE_BYTES {
            return Err(format!("invalid file length {len}").into());
        }
        self.change(wal, |written| {
            // Every page past it a hole: those written in the WAL too, as an
            // earlier shrink may have cut flushed pages there.
            let kept = len.div_ceil(PAGE);
            for (_, image) in written.pages.range_mut(kept..) {
                *image = None;
            }
            for page in kept..written.size.div_ceil(PAGE) {
                written.pages.insert(page, None);
            }
            written.size = len;
        })
    }

    /// Change the file's writes in `wal` with `f`, starting from none at the
    /// file's length as last flushed.
    fn change(&self, wal: &mut Wal, f: impl FnOnce(&mut snapshot::Written)) -> Result<()> {
        wal.of(&self.shared)?;
        check_name(&self.name)?;
        let paged = self.shared.snapshot().is_paged(&self.name);
        if wal.records.contains_key(&*self.name) || paged == Some(false) {
            return Err(format!("{:?} is a record, not a paged file", self.name).into());
        }
        let size = self.len();
        let written = (wal.files.entry(self.name.to_string())).or_insert_with(|| {
            snapshot::Written {
                size,
                pages: Default::default(),
            }
        });
        f(written);
        Ok(())
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

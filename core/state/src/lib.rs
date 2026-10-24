//! Transactional state over an asynchronous key/value store: paged files and
//! records, changed through a write-ahead log in memory, which a flush makes
//! durable at once.
//!
//! A [`State`] is one namespace in a KV store: the state of a Restate object
//! without Restate's journal, which keeps writes in issue order and makes
//! them durable at a sync (see [`kv`]). It needs no multi-key transaction,
//! compare-and-swap or listing operation. Its [`File`]s are 4 KiB pages read
//! and written at random, and its [`Record`]s whole values. The state is
//! read; it changes only through a [`Wal`] ([`State::wal`]): the records set
//! and pages written there become durable together at [`Wal::flush`], and a
//! WAL dropped discards them.
//!
//! What writes a WAL is a [`Participant`]: it keeps changes of its own, moves
//! them into the WAL before the flush, and may leave state idle for the
//! namespace's next [`State`]. The `sql` crate's database is one, on the
//! file `db`.
//!
//! # Copy on write
//!
//! Nothing stored is ever changed: a flush writes what changed as new,
//! immutable blobs, then the root, the one key it overwrites, which names
//! them; then it syncs once. A file is stored as chunks of a few pages, each
//! a zstd frame compressed against the chunk's base, so a flush costs about
//! what it changed. A crash keeps a prefix of a flush's writes, so the root
//! stored names only blobs that are there: there is no log to replay, and
//! nothing to repair. See the private `store` module, and `docs/DESIGN.md`.
//!
//! | Layer | Representation | Survives? |
//! |---|---|---|
//! | A WAL | Records set and pages written in it, in memory | No: dropping it discards them |
//! | Root | `v16/root`: the directory, small records, and the blobs of each file's chunks | Yes, after an acknowledged flush |
//! | Blobs | `v16/blob/<n>`: a chunk's base or delta, or a large record; written once | Yes, until no root a reader may hold names it |
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
//! never change and are deleted only [`StateOptions::garbage_after`] flushes
//! after the last root that named them. Only one flushes: a flush is refused
//! if another state of the owner flushed since this one opened.
//!
//! The [`PageCache`] is the process's: it keeps blobs in memory and on a
//! local disk tier under one budget for every namespace it opens, under
//! their numbers, for as long as it likes: a blob never changes. It
//! remembers the root this owner made durable, so a steady-state open and
//! flush are each one round trip. A participant may leave idle state there
//! ([`Participant::park`]); the next state hands it back when nothing
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
//! wal.set(&state.record("progress"), "step 1".into())?;
//! wal.write(&state.file("image")?, 0, Some(vec![7; 4096].into()))?;
//! wal.flush().await?; // the record and the page, durable together
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! A file holds at most [`MAX_FILE_BYTES`], a record's value
//! [`MAX_RECORD_BYTES`], a name [`MAX_NAME_BYTES`]. The backend is assumed
//! to return what was stored; decoding checks structure and bounds, and
//! blobs' zstd frames carry a checksum. Use a fresh, dedicated namespace; no
//! migration or format discovery is implemented. Simulation and benchmark
//! support require the opt-in `simulation` feature.
mod cache;
pub mod kv;
#[cfg(feature = "simulation")]
pub mod simulation;
mod store;
mod zstd;

pub use cache::{PageCache, PageCacheConfig};
pub use store::{FlushReport, MAX_FILE_BYTES, MAX_NAME_BYTES, MAX_RECORD_BYTES};

use crate::cache::{Idle, NamespaceCache};
use crate::kv::KvStore;
use crate::store::{Settings, Snapshot};
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
    fn id(&self) -> u64;
    /// Page `index` (4096 bytes); `None` past the base's end.
    fn page(&self, index: u64) -> Option<&[u8]>;
}

/// Default [`StateOptions::chunk_pages`].
pub const CHUNK_PAGES: u64 = 4;

/// How a state flushes and reads. Only [`Self::chunk_pages`] is stored, when
/// the namespace is made; the others never change the stored format.
#[derive(Clone)]
pub struct StateOptions {
    /// Pages per chunk, the unit a flush rewrites: smaller chunks cost less
    /// to rebase, larger ones compress better and make a smaller root. Set
    /// when the namespace is made; its root keeps it.
    pub chunk_pages: u64,
    /// A chunk's delta past this fraction of its base's stored bytes is
    /// written as a new base instead.
    pub rebase_ratio: f64,
    /// Bytes of a record the root holds; a larger one is a blob of its own,
    /// written when it changes.
    pub inline_record_bytes: usize,
    /// Every chunk of a file of at most this many pages is fetched at open,
    /// alongside the root's other blobs.
    pub prefetch_pages: u64,
    /// Flushes after the last root that named a blob before a flush deletes
    /// it (at least 1): a reader that opened before reads its root for as
    /// long.
    pub garbage_after: u64,
}

impl Default for StateOptions {
    fn default() -> Self {
        Self {
            chunk_pages: CHUNK_PAGES,
            rebase_ratio: 0.25,
            inline_record_bytes: 1024,
            prefetch_pages: 16,
            garbage_after: 16,
        }
    }
}

/// One namespace's state: its paged files and its records, as last flushed.
/// It is read here, and changed through a [`Wal`] ([`Self::wal`]), which
/// [`Wal::flush`] makes durable in one put. A cheap handle; clones, and the
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
    /// The durable state: as opened, then as the WALs flushed it.
    snapshot: Mutex<Arc<Snapshot>>,
    options: Settings,
    /// The participants that joined, by name: whether each holds changes not
    /// yet in a WAL.
    participants: Mutex<HashMap<String, bool>>,
    /// What participants left idle at the state this one opened on, by
    /// participant, until they take it.
    idle: Mutex<HashMap<String, Idle>>,
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
        let Some(cache) = self
            .options
            .cache
            .as_ref()
            .filter(|_| !*self.reader.get_mut())
        else {
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
    /// Open the namespace over `store`: read its commit record (and the
    /// stored values it names), recovering what a crash left, before
    /// anything reads. Await it as it is, or set how first:
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

    async fn open_as(store: Arc<dyn KvStore>, options: Settings, reader: bool) -> Result<Self> {
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
    pub fn options(&self) -> &StateOptions {
        &self.shared.options.options
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
    /// it ([`Wal::set`], [`Wal::write`]) become durable together at
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

    /// Whether the state opened in a [`PageCache`] ([`Open::cached`]).
    pub fn is_cached(&self) -> bool {
        self.shared.options.cache.is_some()
    }

    /// Whether this is a reader ([`Open::reader`]): its WALs never flush.
    pub fn is_reader(&self) -> bool {
        self.shared.reader.load(Ordering::SeqCst)
    }

    /// Whether the state can flush: a failed or cancelled flush ends it.
    pub fn is_usable(&self) -> bool {
        !self.shared.poisoned.load(Ordering::SeqCst)
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
    /// back what participants left idle ([`Participant::park`]).
    pub fn cached(self, cache: &PageCache, namespace: &str) -> Self {
        Self {
            cache: Some(cache.namespace(namespace)),
            ..self
        }
    }

    /// Open a reader of a namespace that another owns, alongside it: a
    /// state that writes nothing to the store (its WALs refuse to flush). It
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
        Box::pin(State::open_as(store, Settings { options, cache }, reader))
    }
}

/// The writes ahead of a [`State`]: the records set and pages written since
/// [`State::wal`] made it, in memory, which [`Self::flush`] makes durable
/// together, in one put. There is no other way to change the state. A WAL
/// dropped discards its writes; a participant that moved changes into it
/// learns which happened from its [`Receipt`].
pub struct Wal {
    shared: Arc<Shared>,
    records: store::Records,
    files: store::Writes,
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

    /// Replace `record`'s value with this WAL's flush. Refused, before the
    /// WAL changes, is what no flush could store: a value past
    /// [`MAX_RECORD_BYTES`], a name past [`MAX_NAME_BYTES`], or the name of
    /// a paged file.
    pub fn set(&mut self, record: &Record, value: Bytes) -> Result<()> {
        self.of(&record.shared)?;
        if value.len() > MAX_RECORD_BYTES {
            let (len, max) = (value.len(), MAX_RECORD_BYTES);
            return Err(format!("record {:?} of {len} bytes exceeds {max}", record.name).into());
        }
        check_name(&record.name)?;
        let paged = record.shared.snapshot().is_paged(&record.name);
        if self.files.contains_key(&*record.name) || paged == Some(true) {
            return Err(format!("{:?} is a paged file, not a record", record.name).into());
        }
        self.records.insert(record.name.to_string(), value);
        Ok(())
    }

    /// Write `image` (4096 bytes) at `file`'s `page`, or a hole with `None`,
    /// with this WAL's flush, which grows the file to include it if it is
    /// shorter then. Refused, before the WAL changes, is what no flush could
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

    /// Make `file` `len` bytes, a multiple of 512, with this WAL's flush,
    /// whatever its length is then: pages wholly past it become holes, so
    /// regrowing it reads zeros (or its base's pages). Bytes of a last,
    /// partial page past `len` stay as written.
    pub fn set_len(&mut self, file: &File, len: u64) -> Result<()> {
        if !len.is_multiple_of(512) || len > MAX_FILE_BYTES {
            return Err(format!("invalid file length {len}").into());
        }
        self.change(file, |written| {
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
            written.sized = true;
        })
    }

    /// Change `file`'s writes with `f`, starting from none at its length as
    /// last flushed.
    fn change(&mut self, file: &File, f: impl FnOnce(&mut store::Written)) -> Result<()> {
        self.of(&file.shared)?;
        check_name(&file.name)?;
        let paged = file.shared.snapshot().is_paged(&file.name);
        if self.records.contains_key(&*file.name) || paged == Some(false) {
            return Err(format!("{:?} is a record, not a paged file", file.name).into());
        }
        let size = file.len();
        let written = (self.files.entry(file.name.to_string())).or_insert_with(|| store::Written {
            size,
            pages: Default::default(),
            sized: false,
        });
        f(written);
        Ok(())
    }

    /// Make every record set and page written durable: the blobs of the
    /// chunks and records it changes, then the root that names them, then
    /// one sync, in one round trip. Nothing changed: nothing is written. It
    /// is refused while a participant holds changes not yet in a WAL, while
    /// another flush of the state runs, and for writes the state cannot
    /// store; a refused flush writes nothing, and the state goes on. `Ok`
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
        trace(&files, &records);
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

/// Experiment: append each flush's writes to the file `STATE_TRACE`, for
/// `examples/replay.rs`. Per flush: `F`, then `P` name page len image (len 0
/// a hole), `S` name size, `R` name len value; lengths u32 LE, pages u64 LE.
fn trace(files: &store::Writes, records: &store::Records) {
    use std::io::Write;
    let Ok(path) = std::env::var("STATE_TRACE") else {
        return;
    };
    let mut out = vec![b'F'];
    let name = |out: &mut Vec<u8>, name: &str| {
        out.extend((name.len() as u32).to_le_bytes());
        out.extend(name.as_bytes());
    };
    for (file, written) in files {
        out.push(b'S');
        name(&mut out, file);
        out.extend(written.size.to_le_bytes());
        for (page, image) in &written.pages {
            out.push(b'P');
            name(&mut out, file);
            out.extend(page.to_le_bytes());
            let image = image.as_deref().unwrap_or(&[]);
            out.extend((image.len() as u32).to_le_bytes());
            out.extend(image);
        }
    }
    for (record, value) in records {
        out.push(b'R');
        name(&mut out, record);
        out.extend((value.len() as u32).to_le_bytes());
        out.extend(&value[..]);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(&out).unwrap();
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
        let idle = self.state.shared.idle.lock().unwrap().remove(&*self.name)?;
        idle.value.downcast().ok().map(|value| *value)
    }

    /// Whether [`Self::park`] would keep state now: the state has a
    /// [`PageCache`] and is at the owner's last durable flush.
    pub fn can_park(&self) -> bool {
        let state = &self.state;
        let Some(cache) = &state.shared.options.cache else {
            return false;
        };
        !state.is_reader() && state.is_usable() && cache.committed() == Some(state.version())
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
/// ([`Wal::set`]). A cheap handle.
///
/// A value of up to [`StateOptions::inline_log_bytes`] is in the record,
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

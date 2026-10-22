//! A durable volume over an asynchronous key/value store: paged files and
//! records, committed together in one put.
//!
//! A [`Volume`] is one namespace in a KV store: the state of a Restate
//! object without Restate's journal, which keeps writes in issue order and
//! makes them durable at a sync (see [`kv`]). It needs no multi-key
//! transaction, compare-and-swap, listing operation or backend WAL API. Its
//! [`File`]s are 4 KiB pages read and written at random, and its [`Record`]s
//! whole values in the commit record; [`Volume::commit`] makes every change
//! durable at once.
//!
//! What uses a volume is a [`Participant`]: it keeps changes of its own, moves
//! them into the volume's files and records before a commit, and may leave
//! state idle for the namespace's next volume. The `sql` crate's database is
//! one, on the file `db`.
//!
//! # When changes become durable
//!
//! At [`Volume::commit`], and only then: the volume's one put makes every
//! record set and file written since the last commit durable together.
//! Dropping the volume discards them. The host decides when to commit: after
//! each SQL transaction (as SQLite does), or once per turn of everything that
//! took part.
//!
//! | Layer | Representation | Survives dropping the volume? |
//! |---|---|---|
//! | Pending changes | Records set and pages written since the last commit | No |
//! | Commit record | `commit`: the log since the last checkpoint, one transaction per commit, compressed against the previous images; a large one's segments at `log/<n>` | Yes, after an acknowledged commit |
//! | Pages | `page/<file>/<n>`, overwritten in place by checkpoints | Yes |
//!
//! A commit puts the record with one more transaction, usually a single put,
//! then one sync. An unchanged commit performs no mutations. An error or
//! cancellation can have an unknown commit outcome. Settle outstanding backend
//! requests and reopen before deciding whether to retry application work.
//!
//! # Why a log and pages in place?
//!
//! One put is the only atomic operation the backend offers: a crash may keep
//! some of a commit's writes and lose the ones after them. The record turns one
//! put into a multi-page commit: a transaction is visible once a record
//! holding it is. Because the log holds every page a checkpoint has not yet
//! recorded, checkpoints may overwrite `page/<n>` in place, after the record:
//! replay applies the log over whatever they left. This is SQLite's own WAL
//! design, applied to KV keys. There is no page map, no object allocation, no
//! intent and no orphan collection. See `docs/DESIGN.md` and the private
//! `snapshot` and `log` modules.
//!
//! The costs are writing each page twice (log, then checkpoint) and replaying the
//! log on open. A logged page costs about what changed in it since its previous
//! image, so checkpoints can be rare. This is not MVCC: one owner reads
//! and writes the namespace.
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including settlement
//! of cancelled requests before reopening or handing ownership to another session.
//! There is no distributed lock or fence here. A commit queues its record, then,
//! when the log is long, its checkpoint, and syncs once. Open reads the
//! record, and the stored segments its log names, if any.
//!
//! Volumes of one owner may overlap when they share a [`NamespaceCache`]:
//! each reads the state it opened at (a checkpoint waits while another volume
//! holds an older version), and a commit is refused if another
//! volume committed since this one opened. Without a cache they must not
//! overlap.
//!
//! [`StorageOptions::cache`] can hold a [`NamespaceCache`]: one namespace's handle
//! on the process-wide [`PageCache`], which keeps pages in memory and on a local
//! disk tier under one budget for every namespace. It keeps them while the
//! record's epoch (its checkpoint count) is the one they belong to, and
//! remembers the record this owner made durable, so a steady-state open and
//! commit are each one round trip. A participant may leave idle state there
//! ([`Participant::park`]); the next volume hands it back when nothing
//! changed. The disk tier's I/O runs on tokio.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use volume::{Volume, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> volume::Result<()> {
//! let volume = Volume::open(store).await?;
//! volume.record("progress").set("step 1".into());
//! volume.file("image")?.write(0, Some(vec![7; 4096].into()))?;
//! volume.commit().await?; // the record and the page, durable together
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! Log segments are capped at [`MAX_SEGMENT_BYTES`]; larger commits span several.
//! A record's value is capped at [`MAX_RECORD_BYTES`], its name at
//! [`MAX_NAME_BYTES`].
//! The backend is assumed to return what was stored; decoding checks structure
//! and bounds, not checksums. Use a fresh, dedicated namespace; no migration or format discovery is
//! implemented. Simulation and benchmark support require the opt-in `simulation`
//! feature.
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
    CHECKPOINT_AFTER, CHECKPOINT_LOG_BYTES, CommitReport, INLINE_LOG_BYTES, StorageOptions,
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
        atomic::{AtomicBool, Ordering},
    },
};

/// Volume and backend errors returned to the host task.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Bytes of a page of a [`File`].
pub const PAGE_SIZE: usize = 4096;

/// One namespace's volume: its paged files and its records, changed in
/// memory and made durable together by [`Self::commit`], one put. A cheap
/// handle; clones, and the views of [`Self::scoped`], share the pending
/// changes.
///
/// Acquire exclusive ownership of the namespace and settle the preceding
/// owner's requests before opening it; a reader ([`Self::open_reader`]) may
/// open beside the owner.
#[derive(Clone)]
pub struct Volume {
    shared: Arc<Shared>,
    /// Prefixed to every name: a view of part of the volume.
    prefix: Arc<str>,
}

/// What a volume's handles share.
struct Shared {
    /// The committed state: as opened, then as this volume committed it.
    snapshot: Mutex<Arc<Snapshot>>,
    options: StorageOptions,
    /// Records set and files written since the last commit, which the next
    /// one makes durable.
    records: Mutex<log::Records>,
    files: Mutex<snapshot::Writes>,
    /// The participants that joined, by name: whether each holds changes not
    /// yet in the volume.
    participants: Mutex<HashMap<String, bool>>,
    /// What participants left idle at the state this volume opened on, by
    /// participant, until they take it.
    idle: Mutex<HashMap<String, Idle>>,
    /// A reader's ([`Volume::open_reader`]): it never commits.
    reader: AtomicBool,
    /// A commit failed or was cancelled: its outcome is unknown.
    poisoned: AtomicBool,
    /// A commit is running.
    committing: AtomicBool,
}

impl Shared {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.lock().unwrap().clone()
    }
}

/// A running commit. One that fails ends the volume. One dropped midway
/// has an unknown outcome too, and the cache forgets what it knew of the
/// namespace (as a failed sync makes it), so that no other volume commits
/// on top of a state that may be gone.
struct Committing<'a> {
    shared: &'a Shared,
    /// The commit returned, rather than being dropped.
    returned: bool,
    /// It succeeded.
    done: bool,
}

impl Drop for Committing<'_> {
    fn drop(&mut self) {
        self.shared.committing.store(false, Ordering::SeqCst);
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

/// The error of a volume that a failed commit ended.
const ENDED: &str = "the volume ended after a failed or cancelled commit; open a new one";

impl Volume {
    /// Open the namespace with default [`StorageOptions`]: read its commit
    /// record (and the stored values it names), recovering what a crash
    /// left, before anything reads.
    pub async fn open(store: Arc<dyn KvStore>) -> Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }

    /// Open with explicit [`StorageOptions`]. Options never change the stored
    /// format: existing page tags determine decoding. With a [`PageCache`],
    /// what participants of this owner's previous volume left idle waits
    /// for them ([`Participant::take_idle`]) when nothing changed since.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> Result<Self> {
        Self::open_as(store, options, false).await
    }

    /// Open a reader of a namespace that another owns, alongside it: a
    /// volume that writes nothing to the store (its changes stay private,
    /// and it refuses to commit) and keeps no cache. It reads the state as of
    /// open, unless the owner checkpoints meanwhile: [`Self::is_current`],
    /// once it has read, says whether it did.
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
                records: Mutex::default(),
                files: Mutex::default(),
                participants: Mutex::default(),
                idle: Mutex::new(idle),
                reader: AtomicBool::new(reader),
                poisoned: AtomicBool::new(false),
                committing: AtomicBool::new(false),
            }),
            prefix: Arc::from(""),
        })
    }

    /// The options this volume opened with.
    pub fn options(&self) -> &StorageOptions {
        &self.shared.options
    }

    /// The record named `name`: a whole value the commit record holds, which
    /// the next commit makes durable once set (see [`Record`]).
    pub fn record(&self, name: &str) -> Record {
        Record {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        }
    }

    /// The paged file named `name`: 4 KiB pages the next commit makes
    /// durable once written (see [`File`]).
    pub fn file(&self, name: &str) -> Result<File> {
        Ok(File {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
            base: None,
        })
    }

    /// The paged file named `name`, over `base` (see [`Base`]): an absent
    /// page, or a hole, reads as the base's page, and pages are compressed
    /// against it. A file made with a base opens only with that one again
    /// (by [`Base::id`]); its log since the last checkpoint decodes now.
    /// [`Self::file`] opens it without its base, to write holes and set its
    /// length only: emptying it for another base, say.
    pub fn file_with_base(&self, name: &str, base: Arc<dyn Base>) -> Result<File> {
        let mut file = self.file(name)?;
        let mut snapshot = self.shared.snapshot.lock().unwrap();
        *snapshot = Arc::new(snapshot.with_base(file.name(), base.clone())?);
        file.base = Some(base);
        Ok(file)
    }

    /// The part of the volume whose names start with `prefix`, named
    /// without it: an extension's, say (`scoped("fs/")` makes `manifest`
    /// the record `fs/manifest`).
    pub fn scoped(&self, prefix: &str) -> Volume {
        Volume {
            prefix: format!("{}{prefix}", self.prefix).into(),
            ..self.clone()
        }
    }

    /// Take part in the volume as `name`, one at a time per name (see
    /// [`Participant`]).
    pub fn join(&self, name: &str) -> Result<Participant> {
        let name = format!("{}{name}", self.prefix);
        let mut joined = self.shared.participants.lock().unwrap();
        if joined.contains_key(&name) {
            return Err(format!("{name:?} already takes part in this volume").into());
        }
        joined.insert(name.clone(), false);
        Ok(Participant {
            name: name.into(),
            volume: self.clone(),
        })
    }

    /// The commit: every record set and file written since the last one, in
    /// one put (and a checkpoint, when the log is long), then one sync.
    /// Nothing changed: nothing is written. It is refused while a
    /// participant holds changes not yet in the volume, and while another
    /// commit of it runs. What it cannot store (a record too large, a bad
    /// size) is refused before anything is written, and stays pending. `Ok`
    /// holds what it wrote; any other error, or dropping the future, leaves
    /// the outcome unknown and ends the volume: open it again.
    pub async fn commit(&self) -> Result<CommitReport> {
        let shared = &self.shared;
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if shared.reader.load(Ordering::SeqCst) {
            return Err("a reader writes nothing".into());
        }
        if let Some(name) = self.unflushed() {
            return Err(
                format!("{name:?} has changes not yet in the volume: flush it first").into(),
            );
        }
        if shared.committing.swap(true, Ordering::SeqCst) {
            return Err("a commit of this volume is running".into());
        }
        let mut running = Committing {
            shared,
            returned: false,
            done: false,
        };
        let snapshot = shared.snapshot();
        // What it cannot store is refused before anything is written, and
        // stays pending: the volume goes on.
        {
            let (files, records) = (shared.files.lock().unwrap(), shared.records.lock().unwrap());
            if let Err(error) = snapshot.check(&files, &records) {
                (running.returned, running.done) = (true, true);
                return Err(error.into());
            }
        }
        let (files, records) = self.take_pending();
        let result = snapshot.commit(files, records).await;
        running.returned = true;
        let (report, after) = result?;
        *shared.snapshot.lock().unwrap() = Arc::new(after);
        running.done = true;
        Ok(report)
    }

    /// A participant holding changes not yet in the volume.
    fn unflushed(&self) -> Option<String> {
        let joined = self.shared.participants.lock().unwrap();
        let mut unflushed = joined.iter().filter(|(_, unflushed)| **unflushed);
        unflushed.next().map(|(name, _)| name.clone())
    }

    /// Drop the records set and files written since the last commit.
    pub fn discard(&self) {
        self.take_pending();
    }

    /// The writes to files and records since the last commit, which the next
    /// one takes.
    fn take_pending(&self) -> (snapshot::Writes, log::Records) {
        let files = std::mem::take(&mut *self.shared.files.lock().unwrap());
        let records = std::mem::take(&mut *self.shared.records.lock().unwrap());
        (files, records)
    }

    /// Whether files or records changed since the last commit.
    fn has_pending(&self) -> bool {
        !self.shared.files.lock().unwrap().is_empty()
            || !self.shared.records.lock().unwrap().is_empty()
    }

    /// The namespace's version: the commits ever made, as this volume holds
    /// it (opened on, or committed itself).
    pub fn version(&self) -> u64 {
        self.shared.snapshot().version()
    }

    /// Whether the store still holds the state this volume opened on (see
    /// [`Self::open_reader`]): if it does not, what was read may mix that
    /// state with a later checkpoint's pages, so read again in a new one. A
    /// volume's own commits change the state, so this is for one that has
    /// not committed.
    pub async fn is_current(&self) -> Result<bool> {
        Ok(self.shared.snapshot().is_current().await?)
    }

    /// Whether the volume can commit: a failed or cancelled commit ends it.
    pub fn is_usable(&self) -> bool {
        !self.shared.poisoned.load(Ordering::SeqCst)
    }
}

/// Something that takes part in a volume under a name ([`Volume::join`]):
/// it keeps changes of its own (an SQL engine's pages, say), and moves them
/// into the volume's files and records before a commit. While it says it
/// holds changes not yet moved ([`Self::set_unflushed`]), the volume refuses
/// to commit. Dropping it leaves the name free, and its unmoved changes out.
///
/// With a [`PageCache`], it may leave state idle for the namespace's next
/// volume ([`Self::park`]), which that volume's participant of the same
/// name takes back ([`Self::take_idle`]) when nothing changed since.
pub struct Participant {
    name: Arc<str>,
    volume: Volume,
}

impl Participant {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The volume it takes part in.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Say whether it holds changes not yet in the volume.
    pub fn set_unflushed(&self, unflushed: bool) {
        let mut joined = self.volume.shared.participants.lock().unwrap();
        joined.insert(self.name.to_string(), unflushed);
    }

    /// What this participant left idle at the end of the owner's previous
    /// volume, if nothing changed since and it is a `T`.
    pub fn take_idle<T: Any + Send>(&self) -> Option<T> {
        let idle = self
            .volume
            .shared
            .idle
            .lock()
            .unwrap()
            .remove(&*self.name)?;
        idle.value.downcast().ok().map(|value| *value)
    }

    /// Whether [`Self::park`] would keep state now: the volume has a
    /// [`PageCache`], is at the owner's last durable commit, and holds no
    /// changes since.
    pub fn can_park(&self) -> bool {
        let volume = &self.volume;
        let Some(cache) = &volume.shared.options.cache else {
            return false;
        };
        volume.is_usable() && cache.committed() == Some(volume.version()) && !volume.has_pending()
    }

    /// Leave `value`, which corresponds to the volume's committed state, for
    /// the namespace's next volume, weighing `weight` bytes against
    /// [`PageCacheConfig::idle_bytes`]. `Err` gives it back if it cannot be
    /// kept (see [`Self::can_park`]). The cache drops it when anything
    /// changes, or under its budget; one heavier than the whole budget is
    /// not kept. `value` must hold no handle of this volume (a [`File`], a
    /// [`Record`], the [`Volume`]), which would keep it open meanwhile and
    /// hold back the namespace's checkpoints.
    pub fn park<T: Any + Send>(&self, value: T, weight: usize) -> std::result::Result<(), T> {
        if !self.can_park() {
            return Err(value);
        }
        let cache = self.volume.shared.options.cache.as_ref().unwrap();
        let idle = Idle {
            value: Box::new(value),
            version: self.volume.version(),
            weight,
        };
        cache.park(&self.name, idle);
        Ok(())
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        (self.volume.shared.participants.lock().unwrap()).remove(&*self.name);
    }
}

/// A named whole value the commit record holds, replaced whole: the host's
/// state beside its files (a run's progress, an outbox), committed with them
/// in the same put. A cheap handle; clones share the volume's pending
/// changes.
///
/// A value of up to [`StorageOptions::inline_log_bytes`] is in the record,
/// which every commit rewrites; a larger one is a stored value of its own,
/// written when it changes. Either way, opening the volume reads it.
#[derive(Clone)]
pub struct Record {
    name: Arc<str>,
    shared: Arc<Shared>,
}

impl Record {
    /// The value as the volume holds it: set since the last commit, or
    /// committed; empty if never set.
    pub fn get(&self) -> Bytes {
        if let Some(value) = self.shared.records.lock().unwrap().get(&*self.name) {
            return value.clone();
        }
        let snapshot = self.shared.snapshot();
        snapshot
            .records()
            .get(&*self.name)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the value with the next commit (see [`Volume::commit`]).
    pub fn set(&self, value: Bytes) {
        (self.shared.records.lock().unwrap()).insert(self.name.to_string(), value);
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A paged file: 4 KiB pages, read and written at random, made durable by
/// the volume's next commit with its other files and its records. An absent
/// page, never written or past the end, reads as `None` (zeros). A cheap
/// handle; clones share the volume's pending changes.
///
/// The commit record's log holds the changed pages until a checkpoint writes
/// them in place, and the [`PageCache`] keeps them across volumes.
#[derive(Clone)]
pub struct File {
    name: Arc<str>,
    shared: Arc<Shared>,
    base: Option<Arc<dyn Base>>,
}

impl File {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bytes in the file: as written since the last commit, or committed.
    pub fn len(&self) -> u64 {
        match self.shared.files.lock().unwrap().get(&*self.name) {
            Some(written) => written.size,
            None => self.shared.snapshot().file_size(&self.name),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Page `page` as the volume holds it; `None` for zeros (a hole of a
    /// file without a base, or past its end).
    pub async fn read(&self, page: u64) -> Result<Option<Page>> {
        if page.saturating_mul(PAGE) >= self.len() {
            return Ok(None);
        }
        let hole = || {
            let base = self.base.as_ref().and_then(|base| base.page(page));
            base.map(Page::copy_from_slice)
        };
        let written = (self.shared.files.lock().unwrap().get(&*self.name))
            .and_then(|written| written.pages.get(&page).cloned());
        if let Some(image) = written {
            return Ok(image.or_else(hole));
        }
        let snapshot = self.shared.snapshot();
        if page * PAGE >= snapshot.file_size(&self.name) {
            return Ok(hole());
        }
        Ok(snapshot.read(&self.name, page).await?)
    }

    /// Write `image` (4096 bytes) at `page`, or a hole with `None`, growing
    /// the file to include it; the next commit makes it durable.
    pub fn write(&self, page: u64, image: Option<Page>) -> Result<()> {
        if image.as_ref().is_some_and(|image| image.len() != PAGE_SIZE) {
            return Err("a page is 4096 bytes".into());
        }
        let end = page.saturating_add(1).saturating_mul(PAGE);
        if end > log::MAX_DATABASE_BYTES {
            return Err("a page past the largest file".into());
        }
        self.change(|written| {
            written.size = written.size.max(end);
            written.pages.insert(page, image);
        });
        Ok(())
    }

    /// Make the file `len` bytes, a multiple of 512: pages wholly past it
    /// become holes, so regrowing it reads zeros (or its base's pages). Bytes
    /// of a last, partial page past `len` stay as written.
    pub fn set_len(&self, len: u64) -> Result<()> {
        if !len.is_multiple_of(512) || len > log::MAX_DATABASE_BYTES {
            return Err(format!("invalid file length {len}").into());
        }
        self.change(|written| {
            // Every page past it a hole: those written since the last commit
            // too, as an earlier shrink may have cut committed pages there.
            let kept = len.div_ceil(PAGE);
            for (_, image) in written.pages.range_mut(kept..) {
                *image = None;
            }
            for page in kept..written.size.div_ceil(PAGE) {
                written.pages.insert(page, None);
            }
            written.size = len;
        });
        Ok(())
    }

    /// Change the file's writes since the last commit with `f`, starting
    /// from none at its current length.
    fn change(&self, f: impl FnOnce(&mut snapshot::Written)) {
        let size = self.len();
        let mut pending = self.shared.files.lock().unwrap();
        let written = (pending.entry(self.name.to_string())).or_insert_with(|| snapshot::Written {
            size,
            pages: Default::default(),
        });
        f(written);
    }
}

/// Bytes of a page, as the `u64` that offsets and sizes are.
const PAGE: u64 = PAGE_SIZE as u64;

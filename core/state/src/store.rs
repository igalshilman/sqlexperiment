//! The state as stored: a checkpoint, and the tail of flushes since, applied
//! in memory (see [`crate::format`] for what is stored where).
//!
//! **Flush.** A flush encodes its mutation as one tail entry (each page it
//! wrote a zstd frame of its own), puts it and the head, and syncs: one round
//! trip. Its pages join the *overlay*: the pages written since the
//! checkpoint, kept as their frames.
//!
//! **Checkpoint.** Now and then, a flush first writes the durable state as a
//! checkpoint: each frame the overlay changed, rebuilt from its frame in the
//! last checkpoint and the overlay's pages, the records set and the log
//! entries appended since, into one new pack, and a manifest, in Restate,
//! that names those, and every other frame and value where the checkpoints
//! before put them. The live frames and values of packs mostly dead move into it too
//! (compaction). The flush's head then names it, and the overlay empties.
//!
//! **Open.** Get the head; then, together, the checkpoint's manifest and the
//! tail entries since it, which the open applies; then, together, the large
//! records and log entries the manifest stores, and small files' frames. An
//! owner whose cache holds the state of the head it read reads nothing more.
//!
//! **Read.** A page in the overlay is decoded from its frame; any other is
//! sliced from its frame in a pack, fetched with one ranged get, which the
//! cache keeps under its location: a pack never changes, and its name is
//! unique.
use crate::{
    Base, PAGE, Page, StateOptions,
    cache::{self, NamespaceCache},
    format::{
        Checkpoint, Entry, FileChange, FileIndex, Head, Location, LogChange, LogIndex, MAX_DECODED,
        MAX_FILE_BYTES, MAX_FRAME_PAGES, MAX_LOG_ENTRIES, MAX_NAME_BYTES, MAX_NAMES,
        MAX_RECORD_BYTES, Manifest, PackId, Tier, Value, invalid,
    },
    tail::Tiered,
    zstd,
};
use bytes::Bytes;
use futures::{
    StreamExt, TryStreamExt,
    future::{BoxFuture, FutureExt, Shared, WeakShared, try_join_all},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex},
};

/// Decoded frames a state keeps before it drops them all.
const KEPT_FRAMES: usize = 256;
/// Reads of packs, or of pages' references, a state has in flight at once:
/// an open of many large values, or a checkpoint of many frames, waits
/// for some rather than asking the store for all of them together.
const CONCURRENT_READS: usize = 32;
/// Bytes a checkpoint's compaction moves at most, sparsest packs first
/// (one pack at least): the rest move at the checkpoints after it.
const MAX_MOVED_BYTES: u64 = 64 << 20;
/// Ranges a compaction moves from one pack are fetched together while the
/// dead bytes between them are at most this many...
const RUN_GAP: u64 = 64 << 10;
/// ... and the whole fetch is at most this many.
const MAX_RUN: u64 = 8 << 20;

/// The outputs of `futures`, in order, with at most [`CONCURRENT_READS`]
/// of them running at once; the first error. They are collected first, so
/// that the future returned holds them and not the iterator that made
/// them (whose closures would keep it from being `Send`).
fn bounded<T, F: Future<Output = io::Result<T>>>(
    futures: impl IntoIterator<Item = F>,
) -> impl Future<Output = io::Result<Vec<T>>> {
    let futures: Vec<F> = futures.into_iter().collect();
    (futures::stream::iter(futures))
        .buffered(CONCURRENT_READS)
        .try_collect()
}

fn refuse<T>(message: String) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// The records' values, by name.
pub(crate) type Records = BTreeMap<String, Bytes>;

/// Logical page replacements of one file, by page number; `None` is a hole.
pub(crate) type Changes = BTreeMap<u64, Option<Page>>;

/// What a mutation writes to one paged file: the pages it changed, and its size:
/// set ([`crate::Mutation::set_len`]), or else only what the pages written need
/// (the file's length when the mutation first wrote it, grown by its writes),
/// which a flush keeps a longer file past.
#[derive(Clone, Debug, Default)]
pub(crate) struct Written {
    pub size: u64,
    pub pages: Changes,
    /// `size` was set, rather than grown by writes.
    pub sized: bool,
}

impl Written {
    /// The file's size after the flush, the file being `current` bytes now.
    pub fn size_after(&self, current: u64) -> u64 {
        match self.sized {
            true => self.size,
            false => self.size.max(current),
        }
    }
}

/// What a flush writes to paged files, by name.
pub(crate) type Writes = BTreeMap<String, Written>;

/// A log's entries: the index of the first, and each from there, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Entries {
    pub first: u64,
    pub values: Vec<Bytes>,
}

impl Entries {
    /// The index the next entry appended gets.
    pub fn next(&self) -> u64 {
        self.first + self.values.len() as u64
    }
}

/// The logs' entries, by name.
pub(crate) type Logs = BTreeMap<String, Entries>;

/// What a mutation does to one log: entries appended after its last, and
/// the index below which entries are dropped (those appended included).
#[derive(Clone, Debug, Default)]
pub(crate) struct Appended {
    pub values: Vec<Bytes>,
    /// The index the first of `values` took, as the state held the log.
    pub from: u64,
    pub before: u64,
}

/// What a flush does to logs, by name.
pub(crate) type Appends = BTreeMap<String, Appended>;

/// What a name is: one kind, for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Is {
    Record,
    Paged,
    Log,
}

impl Is {
    /// Refuse the name `name`, held as `self`, as `wanted`.
    pub fn refuse(self, name: &str, wanted: Is) -> String {
        format!("{name:?} is {}, not {}", self.article(), wanted.article())
    }

    fn article(self) -> &'static str {
        match self {
            Is::Record => "a record",
            Is::Paged => "a paged file",
            Is::Log => "a log",
        }
    }
}

/// What a flush wrote.
#[derive(Clone, Debug, Default)]
pub struct FlushReport {
    /// Bytes of its tail entry: the records, log entries and pages it wrote.
    pub entry_bytes: usize,
    /// Bytes of the head it put; 0 when nothing changed.
    pub head_bytes: usize,
    /// What the checkpoint its head names did, if it named a new one
    /// ([`crate::StateOptions::checkpoint_entries`]).
    pub checkpoint: Option<Checkpointed>,
    /// Why the checkpoint that was due failed: the flush went on without
    /// it, the tail grows, and the next flush tries again; after more
    /// failures in a row, fewer and fewer flushes do (one in 64 at most).
    pub checkpoint_failed: Option<String>,
}

/// What a checkpoint did ([`FlushReport::checkpoint`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpointed {
    /// The version it holds the state of.
    pub version: u64,
    /// Frames it rebuilt, at a promotion.
    pub frames: usize,
    /// Pages it copied into L0, as the tail wrote them.
    pub pages: usize,
    /// Large records and log entries it wrote.
    pub values: usize,
    /// Bytes of its pack: none if it stored nothing outside its manifest.
    pub pack_bytes: usize,
    /// Bytes of its manifest, in Restate.
    pub manifest_bytes: usize,
    /// Packs whose live ranges it moved into its own, at a promotion: L0's,
    /// and L1's mostly dead.
    pub compacted: usize,
    /// Packs its garbage collection deleted ([`crate::State::collect_garbage`]).
    pub deleted: usize,
    /// Its pack went to L1 (S3), and everything L0 held with it.
    pub promoted: bool,
}

/// A checkpoint put, for the next head to name.
pub(crate) struct Staged {
    pub checkpoint: Checkpoint,
    pub manifest: Arc<Manifest>,
    pub report: Checkpointed,
    /// The frames it rebuilt, decoded.
    pub images: HashMap<Location, Arc<Bytes>>,
}

/// A pack a checkpoint fills: its ranges, back to back, and what it wrote,
/// with the decoded frames among them, for the cache.
struct Pack {
    id: PackId,
    body: Vec<u8>,
    written: Vec<(Location, Bytes, Option<Arc<Bytes>>)>,
}

impl Pack {
    fn new(id: PackId) -> Self {
        Self {
            id,
            body: Vec::new(),
            written: Vec::new(),
        }
    }

    /// The bytes at `at`, which it holds.
    fn slice(&self, at: Location) -> Bytes {
        Bytes::copy_from_slice(&self.body[at.offset as usize..at.range().end as usize])
    }

    /// Append `bytes`, the decoded `image` of a frame if one: where they are.
    fn put(&mut self, bytes: Bytes, image: Option<Arc<Bytes>>) -> Location {
        let at = Location {
            pack: self.id,
            offset: self.body.len() as u64,
            len: bytes.len() as u64,
        };
        self.body.extend_from_slice(&bytes);
        self.written.push((at, bytes, image));
        at
    }
}

/// A paged file as of a head: its size, its base's id, and the pages written
/// since the checkpoint (its overlay), each a frame (`None`: a hole).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FileState {
    pub size: u64,
    pub base: Option<u64>,
    pub pages: BTreeMap<u64, Option<Bytes>>,
    /// The fewest pages the file had since the checkpoint, if it shrank:
    /// the checkpoint's pages from there on are cut, holes.
    pub cut: Option<u64>,
}

impl FileState {
    /// Whether the checkpoint's page `page` is cut.
    fn cut(&self, page: u64) -> bool {
        self.cut.is_some_and(|cut| page >= cut)
    }
}

/// The state as of a head: its checkpoint's manifest, and its tail entries,
/// applied. Immutable once made, so that the cache may keep it for the
/// owner's next open.
#[derive(Clone, Debug)]
pub(crate) struct Durable {
    pub head: Head,
    pub manifest: Arc<Manifest>,
    pub files: BTreeMap<String, FileState>,
    pub records: Records,
    pub logs: Logs,
    /// Records set since the checkpoint.
    pub dirty: BTreeSet<String>,
}

impl Durable {
    /// About the bytes it takes in memory: the values and pages it holds,
    /// and the manifest's locations.
    pub fn estimated_bytes(&self) -> usize {
        const LOCATION: usize = 40;
        let files = (self.manifest.files.values())
            .map(|file| (file.frames.len() + file.pages.len()) * LOCATION)
            .sum::<usize>();
        let pages = (self.files.values())
            .flat_map(|file| file.pages.values())
            .map(|page| page.as_ref().map_or(0, Bytes::len) + LOCATION)
            .sum::<usize>();
        let records = (self.records.iter())
            .map(|(name, value)| name.len() + value.len() + LOCATION)
            .sum::<usize>();
        let logs = (self.logs.values())
            .flat_map(|log| &log.values)
            .map(|value| value.len() + LOCATION)
            .sum::<usize>();
        files + pages + records + logs
    }

    /// The state of `head` with nothing in it.
    pub fn empty(head: Head) -> Self {
        Self {
            head,
            manifest: Arc::default(),
            files: BTreeMap::new(),
            records: Records::new(),
            logs: Logs::new(),
            dirty: BTreeSet::new(),
        }
    }

    /// The state of `head`'s checkpoint, whose manifest is `manifest`, with
    /// the values it stores in `values`.
    fn checkpointed(
        head: Head,
        manifest: Arc<Manifest>,
        values: &HashMap<Location, Bytes>,
    ) -> io::Result<Self> {
        let value = |value: &Value| match value {
            Value::Inline(bytes) => Ok(bytes.clone()),
            Value::Stored(at) => decode_value(&values[at]),
        };
        let records = (manifest.records.iter())
            .map(|(name, record)| Ok((name.clone(), value(record)?)))
            .collect::<io::Result<_>>()?;
        let logs = (manifest.logs.iter())
            .map(|(name, log)| {
                let values = log.values.iter().map(value).collect::<io::Result<_>>()?;
                let first = log.first;
                Ok((name.clone(), Entries { first, values }))
            })
            .collect::<io::Result<_>>()?;
        let frame_bytes = head.frame_bytes();
        let mut files = BTreeMap::new();
        for (name, file) in &manifest.files {
            if file.frames.len() as u64 != file.size.div_ceil(frame_bytes) {
                return Err(invalid("manifest"));
            }
            let state = FileState {
                size: file.size,
                base: file.base,
                ..FileState::default()
            };
            files.insert(name.clone(), state);
        }
        let durable = Self {
            head,
            manifest,
            files,
            records,
            logs,
            dirty: BTreeSet::new(),
        };
        let names = durable.files.len() + durable.records.len() + durable.logs.len();
        match names == durable.names().len() {
            true => Ok(durable),
            false => Err(invalid("manifest: a name of two kinds")),
        }
    }

    /// Every name, and what it is.
    fn names(&self) -> HashMap<&str, Is> {
        (self.records.keys().map(|name| (name.as_str(), Is::Record)))
            .chain(self.files.keys().map(|name| (name.as_str(), Is::Paged)))
            .chain(self.logs.keys().map(|name| (name.as_str(), Is::Log)))
            .collect()
    }

    pub fn is(&self, name: &str) -> Option<Is> {
        if self.records.contains_key(name) {
            Some(Is::Record)
        } else if self.files.contains_key(name) {
            Some(Is::Paged)
        } else {
            self.logs.contains_key(name).then_some(Is::Log)
        }
    }

    /// Apply the tail entry `entry`, which a flush made of this state.
    fn apply(&mut self, entry: &Entry) -> io::Result<()> {
        let kind = |this: &Self, name: &str, wanted: Is| match this.is(name) {
            Some(held) if held != wanted => Err(invalid("tail entry: a name of two kinds")),
            _ => Ok(()),
        };
        for (name, value) in &entry.records {
            kind(self, name, Is::Record)?;
            self.records.insert(name.clone(), value.clone());
            self.dirty.insert(name.clone());
        }
        for (name, change) in &entry.logs {
            kind(self, name, Is::Log)?;
            let held = self.logs.entry(name.clone()).or_default();
            let dropped = (change.before.saturating_sub(held.first)).min(held.values.len() as u64);
            held.values.drain(..dropped as usize);
            held.first = held.first.max(change.before);
            if !change.values.is_empty() && change.from != held.next() {
                return Err(invalid("tail entry: an append out of order"));
            }
            held.values.extend(change.values.iter().cloned());
            if held.values.len() as u64 > MAX_LOG_ENTRIES {
                return Err(invalid("tail entry: a log too long"));
            }
        }
        for (name, change) in &entry.files {
            kind(self, name, Is::Paged)?;
            let file = self.files.entry(name.clone()).or_insert_with(|| FileState {
                base: change.base,
                ..FileState::default()
            });
            if file.base != change.base {
                return Err(invalid("tail entry: a file's base changed"));
            }
            if change.size < file.size {
                let end = change.size.div_ceil(PAGE);
                file.pages.retain(|&page, _| page < end);
                file.cut = Some(file.cut.map_or(end, |cut| cut.min(end)));
            }
            file.size = change.size;
            file.pages.extend(change.pages.iter().cloned());
        }
        match self.files.len() + self.records.len() + self.logs.len() <= MAX_NAMES {
            true => Ok(()),
            false => Err(invalid("tail entry: too many names")),
        }
    }

    /// This state, once the checkpoint `staged` of it is named: the same
    /// files, records and logs, and an empty overlay.
    fn rebased(&self, staged: &Staged) -> Self {
        let mut files = self.files.clone();
        for file in files.values_mut() {
            file.pages.clear();
            file.cut = None;
        }
        Self {
            head: self.head.clone(),
            manifest: staged.manifest.clone(),
            files,
            records: self.records.clone(),
            logs: self.logs.clone(),
            dirty: BTreeSet::new(),
        }
    }
}

/// A record's value, or a log entry, as a pack stores it: a zstd frame.
pub(crate) fn decode_value(frame: &[u8]) -> io::Result<Bytes> {
    zstd::decompress_sized(frame, &[], MAX_RECORD_BYTES).map(Bytes::from)
}

/// What frame `index` of a file holds where nothing is stored: its `base`'s
/// pages, or zeros.
pub(crate) fn implicit(base: Option<&Arc<dyn Base>>, frame_pages: u64, index: u64) -> Vec<u8> {
    let mut pages = vec![0; (frame_pages * PAGE) as usize];
    if let Some(base) = base {
        for (i, page) in pages.chunks_mut(PAGE as usize).enumerate() {
            if let Some(image) = base.page(index * frame_pages + i as u64) {
                page.copy_from_slice(image);
            }
        }
    }
    pages
}

/// The base's page `page`; nothing without a base.
pub(crate) fn base_page(base: Option<&Arc<dyn Base>>, page: u64) -> &[u8] {
    base.and_then(|base| base.page(page)).unwrap_or_default()
}

/// A page written, as a tail entry keeps it: a zstd frame, against its
/// `reference` (see [`Snapshot::reference`]).
fn encode_page(image: &[u8], reference: &[u8]) -> io::Result<Bytes> {
    zstd::compress(image, reference, Bytes::copy_from_slice)
}

pub(crate) fn decode_page(frame: &[u8], reference: &[u8]) -> io::Result<Page> {
    let image = zstd::decompress_sized(frame, reference, PAGE as usize)?;
    match image.len() == PAGE as usize {
        true => Ok(image.into()),
        false => Err(invalid("page")),
    }
}

/// A pack range being fetched, which concurrent reads of it share.
type Fetch = Shared<BoxFuture<'static, Result<Bytes, Arc<io::Error>>>>;
/// A fetch held only by its readers: when all of them are dropped, so is
/// the request.
type Fetching = HashMap<Location, WeakShared<BoxFuture<'static, Result<Bytes, Arc<io::Error>>>>>;

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    tiered: Arc<Tiered>,
    options: StateOptions,
    /// The namespace in the process's [`crate::PageCache`].
    cache: Option<NamespaceCache>,
    /// A reader's: it changes nothing in the cache.
    reader: bool,
    pub(crate) durable: Arc<Durable>,
    /// The bases files were opened with, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
    /// Nothing changed since this owner last saw the namespace.
    pub(crate) unchanged: bool,
    /// Frames decoded, by location: packs never change, so every view of
    /// the state shares them.
    frames: Arc<Mutex<HashMap<Location, Arc<Bytes>>>>,
    fetching: Arc<Mutex<Fetching>>,
}

impl Snapshot {
    /// Open the namespace: see the module documentation. An owner without a
    /// cache, or whose last flush had an unknown outcome, syncs alongside, so
    /// that what it reads is durable; a `reader` reads what is stored.
    pub async fn open(
        tiered: Arc<Tiered>,
        options: StateOptions,
        cache: Option<NamespaceCache>,
        reader: bool,
    ) -> io::Result<Self> {
        if !(1..=MAX_FRAME_PAGES).contains(&options.frame_pages) {
            return refuse(format!("frame_pages {} is invalid", options.frame_pages));
        }
        let owner = cache.as_ref().filter(|_| !reader);
        let unsettled = owner.and_then(NamespaceCache::unsettled);
        let settle = !reader && (owner.is_none() || unsettled.is_some());
        let settle = async {
            match settle {
                true => tiered.restate.sync().await,
                false => Ok(()),
            }
        };
        let (head_bytes, ()) = futures::try_join!(tiered.head(), settle)?;
        if let (Some(owner), Some(unknown)) = (owner, unsettled) {
            owner.settled(unknown);
        }
        let head = match &head_bytes {
            Some(bytes) => Head::decode(bytes)?,
            None => Head::new(cache::random(), options.frame_pages),
        };
        let validated = owner.map(|owner| owner.validate(head_bytes.as_deref()));
        let known = validated.as_ref().is_some_and(|validated| validated.known);
        let reused = owner.filter(|_| known).and_then(NamespaceCache::durable);
        let mut snapshot = Self {
            tiered,
            options,
            cache: cache.clone(),
            reader,
            durable: Arc::new(Durable::empty(head.clone())),
            bases: BTreeMap::new(),
            unchanged: false,
            frames: Arc::default(),
            fetching: Arc::default(),
        };
        snapshot.durable = match reused {
            Some(durable) => durable,
            None => Arc::new(snapshot.load(head).await?),
        };
        if let (Some(owner), Some(validated)) = (owner, validated) {
            let version = snapshot.version();
            snapshot.unchanged = known && owner.committed() == Some(version);
            let committed = (!known).then_some(version);
            owner.record(&validated, head_bytes, committed, snapshot.durable.clone());
        }
        Ok(snapshot)
    }

    /// The state of `head`: its checkpoint's manifest and its tail together,
    /// then the values the manifest stores and small files' frames together.
    async fn load(&self, head: Head) -> io::Result<Durable> {
        let manifest = async {
            match &head.checkpoint {
                Some(checkpoint) => self.manifest(checkpoint).await,
                None => Ok(Manifest::default()),
            }
        };
        let tail = self.tiered.tail(&head, head.checkpointed() + 1);
        let (manifest, entries) = futures::try_join!(manifest, tail)?;
        let values: Vec<Location> = (manifest.records.values())
            .chain(manifest.logs.values().flat_map(|log| &log.values))
            .filter_map(|value| match value {
                Value::Stored(at) => Some(*at),
                Value::Inline(_) => None,
            })
            .collect();
        let small: Vec<&FileIndex> = (manifest.files.values())
            .filter(|file| file.base.is_none())
            .filter(|file| file.size.div_ceil(PAGE) <= self.options.prefetch_pages)
            .collect();
        let wanted: Vec<Location> = (values.iter().copied())
            .chain(
                small
                    .iter()
                    .flat_map(|file| file.frames.iter().flatten().copied()),
            )
            .chain(
                small
                    .iter()
                    .flat_map(|file| file.pages.values().flatten().copied()),
            )
            .collect();
        let fetched = bounded(wanted.iter().copied().map(|at| self.raw(at))).await?;
        let fetched: HashMap<Location, Bytes> = wanted.into_iter().zip(fetched).collect();
        for file in &small {
            let frames: Vec<Option<Arc<Bytes>>> = (file.frames.iter())
                .map(|at| {
                    at.map(|at| self.decode_frame(&fetched[&at], None, head.frame_pages, 0, at))
                })
                .map(Option::transpose)
                .collect::<io::Result<_>>()?;
            for (&page, &at) in &file.pages {
                let Some(at) = at else {
                    continue;
                };
                let frame = frames
                    .get((page / head.frame_pages) as usize)
                    .cloned()
                    .flatten();
                let offset = ((page % head.frame_pages) * PAGE) as usize;
                let reference = frame.map_or(Bytes::new(), |frame| {
                    frame.slice(offset..offset + PAGE as usize)
                });
                self.keep(at, Arc::new(decode_page(&fetched[&at], &reference)?));
            }
        }
        let mut durable = Durable::checkpointed(head, Arc::new(manifest), &fetched)?;
        for entry in &entries {
            durable.apply(entry)?;
        }
        Ok(durable)
    }

    pub fn version(&self) -> u64 {
        self.durable.head.version
    }

    pub fn head(&self) -> &Head {
        &self.durable.head
    }

    pub fn records(&self) -> &Records {
        &self.durable.records
    }

    /// The log `name`'s entries; none if it was never appended to.
    pub fn log(&self, name: &str) -> Entries {
        self.durable.logs.get(name).cloned().unwrap_or_default()
    }

    /// The log `name`'s first index, and the next.
    pub fn log_bounds(&self, name: &str) -> (u64, u64) {
        (self.durable.logs.get(name)).map_or((0, 0), |entries| (entries.first, entries.next()))
    }

    pub fn file_size(&self, name: &str) -> u64 {
        self.durable.files.get(name).map_or(0, |file| file.size)
    }

    /// What the name `name` is, if the state holds it.
    pub(crate) fn is(&self, name: &str) -> Option<Is> {
        self.durable.is(name)
    }

    /// The base the file `name` was opened with: `Err` for a file made with
    /// one that it was not opened with.
    fn base_of(&self, name: &str) -> io::Result<Option<&Arc<dyn Base>>> {
        let made = self.durable.files.get(name).and_then(|file| file.base);
        match (made, self.bases.get(name)) {
            (Some(_), None) => Err(io::Error::other(format!(
                "{name:?} has a base: open it with it"
            ))),
            (_, base) => Ok(base),
        }
    }

    /// Page `page` of the file `name`; `None` for zeros (a hole of a file
    /// without a base, or past its end).
    pub async fn read(&self, name: &str, page: u64) -> io::Result<Option<Page>> {
        let Some(file) = self.durable.files.get(name) else {
            return Ok(None);
        };
        let base = self.base_of(name)?;
        let image = match file.pages.get(&page) {
            Some(None) => None,
            Some(Some(frame)) => {
                let reference = self.reference(&self.durable.manifest, None, name, page);
                Some(decode_page(frame, &reference.await?)?)
            }
            None if file.cut(page) => None,
            None => {
                let manifest = &self.durable.manifest;
                let index = manifest.files.get(name);
                match index.and_then(|index| index.pages.get(&page)) {
                    Some(Some(at)) => Some(self.page(name, page, *at).await?),
                    Some(None) => None,
                    None if index.is_some_and(|index| index.cut(page)) => None,
                    None => self.stored_page(manifest, None, name, page).await?,
                }
            }
        };
        let based = base.is_some_and(|base| base.page(page).is_some());
        Ok(match image {
            // Zeros where the base has no page read as a hole.
            Some(image) if based || image.iter().any(|&byte| byte != 0) => Some(image),
            Some(_) => None,
            None => base
                .and_then(|base| base.page(page))
                .map(Page::copy_from_slice),
        })
    }

    /// Frame `index` of the file `name`, stored at `at`: decoded, or fetched
    /// and decoded (and kept).
    async fn frame(&self, name: &str, index: u64, at: Location) -> io::Result<Arc<Bytes>> {
        if let Some(frame) = self.frames.lock().unwrap().get(&at) {
            return Ok(frame.clone());
        }
        let base = self.base_of(name)?;
        let raw = self.raw(at).await?;
        self.decode_frame(&raw, base, self.durable.head.frame_pages, index, at)
    }

    /// Page `page` of the file `name`, as L0 holds it at `at`: decoded, or
    /// fetched and decoded (and kept). It is compressed against its
    /// reference, as the tail wrote it.
    async fn page(&self, name: &str, page: u64, at: Location) -> io::Result<Page> {
        if let Some(image) = self.frames.lock().unwrap().get(&at) {
            return Ok((**image).clone());
        }
        let stored = self.stored_page(&self.durable.manifest, None, name, page);
        let (raw, stored) = futures::try_join!(self.raw(at), stored)?;
        let reference = match stored {
            Some(stored) => stored,
            None => Bytes::copy_from_slice(base_page(self.base_of(name)?, page)),
        };
        let image = decode_page(&raw, &reference)?;
        self.keep(at, Arc::new(image.clone()));
        Ok(image)
    }

    /// What the tail, and L0, compress page `page` of the file `name`
    /// against, under the checkpoint `manifest`: the page as its frame in L1
    /// holds it, else its base's page, else nothing. Only a promotion
    /// changes it, so every page written since one decodes alone, and an L0
    /// checkpoint copies them as they are. `images` are frames a promotion
    /// just wrote.
    async fn reference(
        &self,
        manifest: &Manifest,
        images: Option<&HashMap<Location, Arc<Bytes>>>,
        name: &str,
        page: u64,
    ) -> io::Result<Bytes> {
        match self.stored_page(manifest, images, name, page).await? {
            Some(stored) => Ok(stored),
            None => Ok(Bytes::copy_from_slice(base_page(self.base_of(name)?, page))),
        }
    }

    /// Page `page` of the file `name` as its frame in `manifest` holds it;
    /// `None` if the frame is not stored.
    async fn stored_page(
        &self,
        manifest: &Manifest,
        images: Option<&HashMap<Location, Arc<Bytes>>>,
        name: &str,
        page: u64,
    ) -> io::Result<Option<Bytes>> {
        let frame_pages = self.durable.head.frame_pages;
        let index = page / frame_pages;
        let stored = (manifest.files.get(name))
            .and_then(|file| file.frames.get(index as usize).copied().flatten());
        let Some(at) = stored else {
            return Ok(None);
        };
        let frame = match images.and_then(|images| images.get(&at)) {
            Some(image) => image.clone(),
            None => self.frame(name, index, at).await?,
        };
        let at = ((page % frame_pages) * PAGE) as usize;
        Ok(Some(frame.slice(at..at + PAGE as usize)))
    }

    /// Decode frame `index` of a file with `base`, as stored at `at`, and
    /// keep it.
    fn decode_frame(
        &self,
        raw: &[u8],
        base: Option<&Arc<dyn Base>>,
        frame_pages: u64,
        index: u64,
        at: Location,
    ) -> io::Result<Arc<Bytes>> {
        let prefix = match base {
            Some(_) => implicit(base, frame_pages, index),
            None => Vec::new(),
        };
        let size = (frame_pages * PAGE) as usize;
        let mut image = Vec::with_capacity(size);
        zstd::decompress(&mut image, raw, &prefix)?;
        if image.len() != size {
            return Err(invalid("frame"));
        }
        let image = Arc::new(Bytes::from(image));
        self.keep(at, image.clone());
        Ok(image)
    }

    /// Keep a decoded frame, dropping them all first once there are
    /// [`KEPT_FRAMES`].
    fn keep(&self, at: Location, image: Arc<Bytes>) {
        let mut frames = self.frames.lock().unwrap();
        if frames.len() >= KEPT_FRAMES {
            frames.clear();
        }
        frames.insert(at, image);
    }

    /// The manifest of `checkpoint`, from the cache or Restate.
    async fn manifest(&self, checkpoint: &Checkpoint) -> io::Result<Manifest> {
        if let Some(cache) = &self.cache
            && let Some(encoded) = cache.manifest(checkpoint.pack).await
        {
            return Manifest::decode(&encoded);
        }
        let encoded = self.tiered.encoded_manifest(checkpoint).await?;
        if let Some(cache) = &self.cache {
            cache.insert_manifest(checkpoint.pack, encoded.clone());
        }
        Manifest::decode(&encoded)
    }

    /// The bytes at `at`, in a pack: from the cache, or the pack store.
    /// Where the store reads in blocks (the parts of a pack kept in
    /// Restate), it fetches and caches the blocks `at` overlaps, so that
    /// its neighbours in them are read from the cache too.
    async fn raw(&self, at: Location) -> io::Result<Bytes> {
        if let Some(cache) = &self.cache
            && let Some(value) = cache.get(at).await
        {
            return Ok(value);
        }
        let Some(block) = self.tiered.block(at.pack) else {
            return self.fetch(at, false).await;
        };
        let blocks = at.offset / block..at.range().end.div_ceil(block);
        let first = blocks.start;
        let got = try_join_all(blocks.map(|index| {
            let at = Location {
                pack: at.pack,
                offset: index * block,
                len: block,
            };
            self.fetch(at, true)
        }))
        .await?;
        let start = (at.offset - first * block) as usize;
        let end = start + at.len as usize;
        let joined = match got.as_slice() {
            [one] => one.clone(),
            _ => Bytes::from(got.concat()),
        };
        if end > joined.len() {
            return Err(io::Error::other(format!(
                "{} is shorter than {:?}",
                at.pack.key(),
                at.range()
            )));
        }
        Ok(joined.slice(start..end))
    }

    /// The bytes at each of `ats`, sorted by pack and offset, for a
    /// compaction: ranges of a pack close together are fetched as one run,
    /// which is not cached whole (each range is, where it moves to).
    async fn raw_runs(&self, ats: &[Location]) -> io::Result<Vec<Bytes>> {
        let mut runs: Vec<(Location, Vec<usize>)> = Vec::new();
        for (i, at) in ats.iter().enumerate() {
            match runs.last_mut() {
                Some((run, members))
                    if run.pack == at.pack
                        && at.offset <= run.range().end + RUN_GAP
                        && at.range().end - run.offset <= MAX_RUN =>
                {
                    run.len = run.len.max(at.range().end - run.offset);
                    members.push(i);
                }
                _ => runs.push((*at, vec![i])),
            }
        }
        let fetched = bounded(runs.iter().map(|&(run, _)| async move {
            match self.tiered.block(run.pack) {
                // In parts, which the cache keeps: a range of them.
                Some(_) => self.raw(run).await,
                None => self.tiered.get(run.pack, run.range()).await,
            }
        }))
        .await?;
        let mut out = vec![Bytes::new(); ats.len()];
        for ((run, members), bytes) in runs.iter().zip(fetched) {
            for &i in members {
                let start = (ats[i].offset - run.offset) as usize;
                out[i] = bytes.slice(start..start + ats[i].len as usize);
            }
        }
        Ok(out)
    }

    /// The range, or with `block` the block, `at` of a pack: from the
    /// cache, or fetched, once for every read that awaits it, and cached.
    async fn fetch(&self, at: Location, block: bool) -> io::Result<Bytes> {
        let cache = self.cache.clone();
        if block
            && let Some(cache) = &cache
            && let Some(value) = cache.get(at).await
        {
            return Ok(value);
        }
        let fetch = {
            let mut fetching = self.fetching.lock().unwrap();
            if let Some(fetch) = fetching.get(&at).and_then(WeakShared::upgrade) {
                fetch
            } else {
                let tiered = self.tiered.clone();
                let fetch: Fetch = async move {
                    let value = match block {
                        true => tiered.get_block(at.pack, at.offset / at.len).await,
                        false => tiered.get(at.pack, at.range()).await,
                    };
                    let value = value.map_err(Arc::new)?;
                    // A pack never changes, and its name is never reused.
                    if let Some(cache) = cache {
                        cache.insert(at, value.clone());
                    }
                    Ok(value)
                }
                .boxed()
                .shared();
                fetching.insert(at, fetch.downgrade().expect("not polled yet"));
                fetch
            }
        };
        let value = fetch.await;
        // Gone once no read awaits it, unless a newer fetch took its place.
        let mut fetching = self.fetching.lock().unwrap();
        if (fetching.get(&at)).is_some_and(|fetch| fetch.upgrade().is_none()) {
            fetching.remove(&at);
        }
        drop(fetching);
        value.map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }

    /// Refuse what no flush could store, before anything is written.
    pub(crate) async fn check(
        &self,
        writes: &Writes,
        records: &Records,
        logs: &Appends,
    ) -> io::Result<()> {
        // Each name is one kind: as stored, and in the mutation.
        let names = (records.keys().map(|name| (name, Is::Record)))
            .chain(writes.keys().map(|name| (name, Is::Paged)))
            .chain(logs.keys().map(|name| (name, Is::Log)));
        let mut seen: HashMap<&str, Is> = HashMap::new();
        for (name, wanted) in names {
            if name.len() > MAX_NAME_BYTES {
                return refuse(format!("name {name:?} exceeds {MAX_NAME_BYTES} bytes"));
            }
            let held = seen.insert(name, wanted).or(self.is(name));
            if let Some(held) = held.filter(|&held| held != wanted) {
                return refuse(held.refuse(name, wanted));
            }
        }
        for (name, value) in records.iter().chain(
            logs.iter()
                .flat_map(|(name, log)| log.values.iter().map(move |value| (name, value))),
        ) {
            if value.len() > MAX_RECORD_BYTES {
                let (len, max) = (value.len(), MAX_RECORD_BYTES);
                return refuse(format!("{name:?}: a value of {len} bytes exceeds {max}"));
            }
        }
        for (name, appended) in logs {
            let held = self.log(name);
            if !appended.values.is_empty() && appended.from != held.next() {
                // The indexes the appends returned would no longer hold.
                return refuse(format!("{name:?} was appended to since"));
            }
            let next = held.next() + appended.values.len() as u64;
            if appended.before > next {
                return refuse(format!("{name:?}: truncating past its end"));
            }
            if next - appended.before.max(held.first) > MAX_LOG_ENTRIES {
                return refuse(format!(
                    "{name:?} would hold more than {MAX_LOG_ENTRIES} entries"
                ));
            }
        }
        for (name, written) in writes {
            let size = written.size_after(self.file_size(name));
            if !size.is_multiple_of(512) || size > MAX_FILE_BYTES {
                return refuse(format!("{name:?}: invalid size {size}"));
            }
            if (written.pages.keys()).any(|&page| page >= MAX_FILE_BYTES / PAGE) {
                return refuse(format!("{name:?}: a page past the largest file"));
            }
            // A checkpoint rebuilds its frames against its base; an emptied
            // file has none left.
            if size > 0 {
                self.base_of(name)?;
            }
        }
        // Its tail entry must decode: refused here, before anything is
        // written, is a mutation whose entry could pass the limit (a page
        // compresses to at most its bytes and 128 more).
        let named = |name: &String| name.len() + 64;
        let bound: usize =
            (records.iter())
                .map(|(name, value)| named(name) + value.len())
                .chain(logs.iter().map(|(name, log)| {
                    named(name)
                        + log
                            .values
                            .iter()
                            .map(|value| value.len() + 16)
                            .sum::<usize>()
                }))
                .chain((writes.iter()).map(|(name, written)| {
                    named(name) + written.pages.len() * (PAGE as usize + 144)
                }))
                .sum();
        if bound > MAX_DECODED {
            return refuse(format!(
                "a mutation of up to {bound} bytes exceeds the {MAX_DECODED} a flush stores"
            ));
        }
        let new = (records.keys().chain(writes.keys()).chain(logs.keys()))
            .filter(|name| self.is(name).is_none())
            .count();
        if self.durable.names().len() + new > MAX_NAMES {
            return refuse("too many files".into());
        }
        Ok(())
    }

    /// Drop from a mutation what changes nothing; whether anything is left.
    pub(crate) fn changes(
        &self,
        writes: &mut Writes,
        records: &mut Records,
        logs: &mut Appends,
    ) -> bool {
        records.retain(|name, value| {
            (self.records().get(name)).map_or(!value.is_empty(), |held| held != value)
        });
        writes.retain(|name, written| {
            let current = self.file_size(name);
            !written.pages.is_empty() || written.size_after(current) != current
        });
        logs.retain(|name, appended| {
            !appended.values.is_empty() || appended.before > self.log(name).first
        });
        !(writes.is_empty() && records.is_empty() && logs.is_empty())
    }

    /// Whether the tail is long enough to checkpoint before the next flush.
    pub(crate) fn due(&self) -> bool {
        let head = self.head();
        let since = head.version - head.checkpointed();
        let (entries, bytes) = (
            self.options.checkpoint_entries,
            self.options.checkpoint_bytes,
        );
        // A checkpoint copies the tail into L0, in Restate, and rewrites the
        // manifest: one is due once the tail passes what L0 and the manifest
        // hold, as it costs more to read, and to write against its pages,
        // than a checkpoint. A tail under a frame's bytes is never worth
        // one: a checkpoint, and the collection after it, costs a round trip
        // or two.
        let manifest = head.checkpoint.map_or(0, |checkpoint| checkpoint.manifest);
        let held = self.tier_bytes(Tier::L0) + manifest;
        let frame = head.frame_pages * PAGE;
        let limit = bytes.min(held.max(frame));
        head.version > 0
            && ((entries > 0 && since >= entries) || (bytes > 0 && head.tail_bytes >= limit))
    }

    /// Bytes the packs of `tier` hold.
    fn tier_bytes(&self, tier: Tier) -> u64 {
        (self.durable.manifest.sizes.iter())
            .filter(|(pack, _)| pack.tier == tier)
            .map(|(_, &bytes)| bytes)
            .sum()
    }

    /// Whether the next checkpoint is a promotion, into L1: once what it
    /// would rebuild or reclaim (L0, the tail it would copy, and the dead
    /// bytes of L1's packs under half live) is as much as L1 holds live, but
    /// at least a frame's bytes and at most [`StateOptions::l0_bytes`]. A
    /// small state rebuilds its frames about as often as it checkpoints; a
    /// large one keeps up to `l0_bytes` of pages in Restate between uploads.
    fn promotes(&self) -> bool {
        let head = &self.durable.head;
        let manifest = &self.durable.manifest;
        let mut live: HashMap<PackId, u64> = HashMap::new();
        for at in manifest.locations() {
            *live.entry(at.pack).or_default() += at.len;
        }
        let (mut held, mut dead) = (0, 0);
        for (pack, &size) in manifest
            .sizes
            .iter()
            .filter(|(pack, _)| pack.tier == Tier::L1)
        {
            let live = live.get(pack).copied().unwrap_or(0);
            held += live;
            if live * 2 < size {
                dead += size - live;
            }
        }
        let l0 = self.tier_bytes(Tier::L0) + head.tail_bytes;
        let frame = head.frame_pages * PAGE;
        l0 + dead >= held.max(frame).min(self.options.l0_bytes)
    }

    /// Flush `writes`, `records` and `logs`, which [`Self::check`] passed,
    /// with a head that names `staged`, a checkpoint of this state, if
    /// given: see the module documentation.
    pub(crate) async fn commit(
        &self,
        mut writes: Writes,
        mut records: Records,
        mut logs: Appends,
        staged: Option<Staged>,
    ) -> io::Result<(FlushReport, Self)> {
        if !self.changes(&mut writes, &mut records, &mut logs) {
            return Ok((FlushReport::default(), self.continued(self.durable.clone())));
        }
        let mut entry = Entry::default();
        // The entry builds on the checkpoint its head names.
        let (manifest, images) = match &staged {
            Some(staged) => (&staged.manifest, Some(&staged.images)),
            None => (&self.durable.manifest, None),
        };
        for (name, written) in writes {
            let held = self.durable.files.get(&name);
            let base = self.bases.get(&name);
            let size = written.size_after(held.map_or(0, |file| file.size));
            // A page past the end is a hole.
            let written: Vec<(u64, Option<Page>)> = (written.pages.into_iter())
                .map(|(page, image)| (page, image.filter(|_| page * PAGE < size)))
                .collect();
            let mut references = Vec::new();
            for (page, image) in &written {
                if image.is_some() {
                    references.push(self.reference(manifest, images, &name, *page));
                }
            }
            let references = bounded(references).await?;
            let mut references = references.into_iter();
            let mut pages = Vec::with_capacity(written.len());
            for (page, image) in written {
                let frame = match image {
                    Some(image) => {
                        let reference = references.next().expect("one a page");
                        Some(encode_page(&image, &reference)?)
                    }
                    None => None,
                };
                pages.push((page, frame));
            }
            let made = held.map_or_else(|| base.map(|base| base.id()), |file| file.base);
            let change = FileChange {
                size,
                base: made,
                pages,
            };
            entry.files.push((name, change));
        }
        entry.records = records.into_iter().collect();
        for (name, appended) in logs {
            let (mut from, mut values) = (appended.from, appended.values);
            if values.is_empty() {
                from = self.log(&name).next();
            }
            // Appended and truncated at once: never stored.
            let dropped = (appended.before.saturating_sub(from)).min(values.len() as u64);
            values.drain(..dropped as usize);
            from += dropped;
            let change = LogChange {
                from,
                values,
                before: appended.before,
            };
            entry.logs.push((name, change));
        }
        let parts = entry.encode()?;
        let entry_bytes: usize = parts.iter().map(Bytes::len).sum();
        let mut next = match &staged {
            Some(staged) => self.durable.rebased(staged),
            None => (*self.durable).clone(),
        };
        next.apply(&entry)?;
        let held = &self.durable.head;
        let mut head = Head {
            version: held.version + 1,
            tail_bytes: held.tail_bytes + entry_bytes as u64,
            ..held.clone()
        };
        head.parts.push(parts.len() as u32);
        let mut deletes = Vec::new();
        if let Some(staged) = &staged {
            // The tail the checkpoint before covered goes; the tail this one
            // covers stays, for readers that opened on it.
            if let Some(before) = held.checkpoint {
                for version in held.tail_first..=before.version() {
                    deletes.extend(held.entry_keys(version));
                }
                head.parts
                    .drain(..(before.version() + 1 - held.tail_first) as usize);
                head.tail_first = before.version() + 1;
            }
            head.previous = held.checkpoint;
            head.checkpoint = Some(staged.checkpoint);
            head.tail_bytes = entry_bytes as u64;
        }
        next.head = head;
        let cache = self.cache.as_ref();
        if cache.is_some_and(|cache| !cache.claim(self.version())) {
            return refuse("another state flushed since this one opened; reopen".into());
        }
        if let Err(error) = self.tiered.write(&next.head, parts, &deletes).await {
            return Err(self.failed(error));
        }
        let head_bytes = next.head.encode();
        let next = Arc::new(next);
        if let Some(cache) = cache {
            cache.flushed(head_bytes.clone(), next.head.version, next.clone());
        }
        let report = FlushReport {
            entry_bytes,
            head_bytes: head_bytes.len(),
            checkpoint: staged.map(|staged| staged.report),
            checkpoint_failed: None,
        };
        Ok((report, self.continued(next)))
    }

    /// Write this state, the head's, as a checkpoint, into one new pack, put,
    /// for the next head to name. Into L0, it copies the pages the overlay
    /// holds as the tail wrote them; a promotion, into L1, rebuilds every
    /// frame L0 or the overlay changed, and takes along what L0 held, and
    /// what L1's packs mostly dead hold. Either stores the records set since
    /// the last checkpoint and the log entries appended since.
    pub(crate) async fn checkpoint(&self) -> io::Result<Staged> {
        let durable = &self.durable;
        let head = &durable.head;
        let promoted = self.promotes();
        let mut pack = Pack::new(PackId {
            version: head.version,
            nonce: cache::random(),
            tier: match promoted {
                true => Tier::L1,
                false => Tier::L0,
            },
        });
        let old = &durable.manifest;
        let mut manifest = Manifest {
            files: old.files.clone(),
            records: old.records.clone(),
            logs: BTreeMap::new(),
            sizes: BTreeMap::new(),
        };
        let (frames, pages) = match promoted {
            false => (0, self.copy_pages(&mut manifest, &mut pack)),
            // A promotion copies the overlay into L0 too, in memory, then
            // folds L0 into the frames.
            true => {
                let mut overlay = Pack::new(PackId {
                    tier: Tier::L0,
                    ..pack.id
                });
                self.copy_pages(&mut manifest, &mut overlay);
                (self.fold(&mut manifest, &overlay, &mut pack).await?, 0)
            }
        };
        let before = pack.written.len();
        // Values: inline if small, else a frame of their own.
        let inline = self.options.inline_record_bytes;
        let store = |pack: &mut Pack, value: &Bytes| -> io::Result<Value> {
            if value.len() <= inline {
                return Ok(Value::Inline(value.clone()));
            }
            let frame = zstd::compress(value, &[], Bytes::copy_from_slice)?;
            Ok(Value::Stored(pack.put(frame, None)))
        };
        for (name, value) in &durable.records {
            if durable.dirty.contains(name) || !manifest.records.contains_key(name) {
                let value = store(&mut pack, value)?;
                manifest.records.insert(name.clone(), value);
            }
        }
        for (name, log) in &durable.logs {
            let held = old.logs.get(name);
            let mut values = Vec::with_capacity(log.values.len());
            for (index, value) in (log.first..).zip(&log.values) {
                let kept = held.and_then(|held| {
                    let at = index.checked_sub(held.first)?;
                    held.values.get(at as usize).cloned()
                });
                values.push(match kept {
                    Some(kept) => kept,
                    None => store(&mut pack, value)?,
                });
            }
            let first = log.first;
            manifest
                .logs
                .insert(name.clone(), LogIndex { first, values });
        }
        let values = pack.written.len() - before;
        let compacted = match promoted {
            true => self.compact(&mut manifest, &mut pack).await?,
            false => 0,
        };
        for held in manifest.packs() {
            if held != pack.id {
                manifest
                    .sizes
                    .insert(held, old.sizes.get(&held).copied().unwrap_or(0));
            }
        }
        // A checkpoint that stores nothing outside its manifest (the values
        // it set are inline) puts no pack.
        let Pack { id, body, written } = pack;
        if !body.is_empty() {
            manifest.sizes.insert(id, body.len() as u64);
        }
        let encoded = manifest.encode()?;
        let checkpoint = Checkpoint {
            pack: id,
            manifest: encoded.len() as u64,
        };
        let report = Checkpointed {
            version: head.version,
            frames,
            pages,
            values,
            pack_bytes: body.len(),
            manifest_bytes: encoded.len(),
            compacted,
            deleted: 0,
            promoted,
        };
        if !body.is_empty() {
            self.tiered.put_pack(id, body.into()).await?;
        }
        self.tiered.put_manifest(id, encoded.clone()).await?;
        // The pack's name is never reused, and no head names it before it is
        // durable: what it holds may be kept, its manifest too (the next
        // writer's open reads it).
        if let Some(cache) = &self.cache {
            cache.insert_manifest(id, encoded);
        }
        let mut images = HashMap::new();
        for (at, raw, image) in written {
            if let Some(cache) = &self.cache {
                cache.insert(at, raw);
            }
            if let Some(image) = image {
                self.keep(at, image.clone());
                images.insert(at, image);
            }
        }
        Ok(Staged {
            checkpoint,
            manifest: Arc::new(manifest),
            report,
            images,
        })
    }

    /// Into L0: each page the overlay holds, copied as the tail wrote it
    /// (its reference stays), or a hole; the overlay's cuts. How many pages.
    fn copy_pages(&self, manifest: &mut Manifest, pack: &mut Pack) -> usize {
        let frame_bytes = self.durable.head.frame_bytes();
        let mut copied = 0;
        for (name, file) in &self.durable.files {
            let index = (manifest.files.entry(name.clone())).or_insert_with(|| FileIndex {
                base: file.base,
                ..FileIndex::default()
            });
            let end = file.size.div_ceil(PAGE);
            index.size = file.size;
            index
                .frames
                .resize(file.size.div_ceil(frame_bytes) as usize, None);
            // A cut since the last checkpoint cuts L0's pages too: those
            // written after it are in the overlay.
            if let Some(cut) = file.cut {
                index.cut = Some(index.cut.map_or(cut, |held| held.min(cut)));
                index.pages.retain(|&page, _| page < cut);
            }
            let stored = index.frames.len() as u64 * self.durable.head.frame_pages;
            index.cut = index.cut.filter(|&cut| cut < stored);
            index.pages.retain(|&page, _| page < end);
            for (&page, frame) in file.pages.range(..end) {
                let at = frame.as_ref().map(|frame| pack.put(frame.clone(), None));
                index.pages.insert(page, at);
                copied += 1;
            }
        }
        copied
    }

    /// Into L1: every frame with a page in L0, or a page cut, rebuilt from
    /// its frame and those pages, each decoded against the frame's page (its
    /// reference); L0 and the cuts emptied. `overlay` holds the pages the
    /// tail wrote, which [`Self::copy_pages`] put in L0. How many frames it
    /// stored.
    async fn fold(
        &self,
        manifest: &mut Manifest,
        overlay: &Pack,
        pack: &mut Pack,
    ) -> io::Result<usize> {
        let frame_pages = self.durable.head.frame_pages;
        let mut dirty = Vec::new();
        for (name, file) in &mut manifest.files {
            let mut changed: BTreeSet<u64> =
                (file.pages.keys()).map(|page| page / frame_pages).collect();
            if let Some(cut) = file.cut {
                let stored = (file.frames.iter().enumerate())
                    .filter(|&(index, at)| at.is_some() && (index as u64 + 1) * frame_pages > cut)
                    .map(|(index, _)| index as u64);
                changed.extend(stored);
            }
            let pages = std::mem::take(&mut file.pages);
            let cut = file.cut.take();
            for index in changed {
                let range = index * frame_pages..(index + 1) * frame_pages;
                let pages: Vec<(u64, Option<Location>)> =
                    pages.range(range).map(|(&page, &at)| (page, at)).collect();
                let stored = file.frames[index as usize];
                dirty.push((name.clone(), file.size, cut, index, stored, pages));
            }
        }
        // The old frames and the pages, fetched together.
        let fetched = bounded(
            dirty
                .iter()
                .map(|(name, _, _, index, stored, pages)| async move {
                    let old = match stored {
                        Some(at) => Some(self.frame(name, *index, *at).await?),
                        None => None,
                    };
                    let raws =
                        bounded(pages.iter().filter_map(|&(_, at)| at).map(|at| async move {
                            match at.pack == overlay.id {
                                true => Ok(overlay.slice(at)),
                                false => self.raw(at).await,
                            }
                        }))
                        .await?;
                    Ok::<_, io::Error>((old, raws))
                }),
        )
        .await?;
        let mut frames = 0;
        for ((name, size, cut, index, _, pages), (old, raws)) in dirty.into_iter().zip(fetched) {
            let base = self.base_of(&name)?;
            let hole = implicit(base, frame_pages, index);
            let mut image = old
                .as_ref()
                .map_or_else(|| hole.clone(), |old| old.to_vec());
            let mut raws = raws.into_iter();
            let pages: BTreeMap<u64, Option<Bytes>> = (pages.into_iter())
                .map(|(page, at)| (page, at.and_then(|_| raws.next())))
                .collect();
            let first = index * frame_pages;
            let end = size.div_ceil(PAGE);
            for page in first..first + frame_pages {
                let at = ((page - first) * PAGE) as usize..((page - first + 1) * PAGE) as usize;
                let written = match pages.get(&page) {
                    Some(frame) => Some(frame.as_ref()),
                    None if cut.is_some_and(|cut| page >= cut) => Some(None),
                    None => None,
                };
                match written {
                    Some(Some(frame)) if page < end => {
                        // Its reference: the page as the old frame holds it.
                        let reference = match &old {
                            Some(_) => &image[at.clone()],
                            None => base_page(base, page),
                        };
                        let decoded = decode_page(frame, reference)?;
                        image[at].copy_from_slice(&decoded);
                    }
                    None if page < end => {}
                    _ => image[at.clone()].copy_from_slice(&hole[at]),
                }
            }
            let stored = match image == hole {
                true => None,
                false => {
                    let prefix = if base.is_some() { &hole[..] } else { &[] };
                    let frame = zstd::compress(&image, prefix, Bytes::copy_from_slice)?;
                    frames += 1;
                    Some(pack.put(frame, Some(Arc::new(Bytes::from(image)))))
                }
            };
            manifest.files.get_mut(&name).expect("indexed").frames[index as usize] = stored;
        }
        Ok(frames)
    }

    /// At a promotion, what L0's packs and L1's packs mostly dead still hold
    /// moves into the new pack: all of L0, and of L1 the cheapest to move
    /// first, up to a budget, so that no promotion copies a large pack's
    /// worth at once. How many packs.
    async fn compact(&self, manifest: &mut Manifest, pack: &mut Pack) -> io::Result<usize> {
        let old = &self.durable.manifest;
        let mut live: HashMap<PackId, u64> = HashMap::new();
        for at in manifest.locations().filter(|at| at.pack != pack.id) {
            *live.entry(at.pack).or_default() += at.len;
        }
        let mut sparse: Vec<(u64, PackId)> = (live.iter())
            .filter(|&(held, _)| held.tier == Tier::L1)
            .filter(|&(held, &live)| live * 2 < old.sizes.get(held).copied().unwrap_or(0))
            .map(|(&held, &live)| (live, held))
            .collect();
        sparse.sort();
        let mut budget = 0;
        let mut moved_out: BTreeSet<PackId> = (sparse.into_iter())
            .take_while(|&(live, _)| {
                let first = budget == 0;
                budget += live;
                first || budget <= MAX_MOVED_BYTES
            })
            .map(|(_, held)| held)
            .collect();
        moved_out.extend(live.keys().filter(|held| held.tier == Tier::L0));
        let moving: BTreeSet<Location> = (manifest.locations())
            .filter(|at| moved_out.contains(&at.pack))
            .copied()
            .collect();
        let moving: Vec<Location> = moving.into_iter().collect();
        let fetched = self.raw_runs(&moving).await?;
        let mut moved = HashMap::new();
        for (at, raw) in moving.into_iter().zip(fetched) {
            moved.insert(at, pack.put(raw, None));
        }
        manifest.relocate(&moved);
        Ok(moved_out.len())
    }

    fn failed(&self, error: io::Error) -> io::Error {
        if let Some(cache) = &self.cache {
            cache.forget();
        }
        error
    }

    fn continued(&self, durable: Arc<Durable>) -> Self {
        Self {
            tiered: self.tiered.clone(),
            options: self.options.clone(),
            cache: self.cache.clone(),
            reader: self.reader,
            durable,
            bases: self.bases.clone(),
            unchanged: false,
            frames: self.frames.clone(),
            fetching: self.fetching.clone(),
        }
    }

    /// This snapshot, with the bases `of` holds that it lacks: those given
    /// while a flush ran, which its snapshot replaces.
    pub(crate) fn keeping_bases(mut self, of: &Self) -> Self {
        for (name, base) in &of.bases {
            (self.bases.entry(name.clone())).or_insert_with(|| base.clone());
        }
        self
    }

    pub(crate) fn with_base(&self, name: &str, base: Arc<dyn Base>) -> io::Result<Self> {
        if base.id() == u64::MAX {
            return refuse("a base's id must be below u64::MAX".into());
        }
        if self.is(name).is_some() {
            let made = self.durable.files.get(name).and_then(|file| file.base);
            if made != Some(base.id()) {
                return refuse(format!("{name:?} was made with another base, or none"));
            }
        }
        let mut with = self.continued(self.durable.clone());
        with.unchanged = self.unchanged;
        with.bases.insert(name.to_owned(), base);
        Ok(with)
    }
}

//! The stored state: immutable blobs, and one root that names them.
//!
//! | Key        | Holds                                                                    |
//! | ---------- | ------------------------------------------------------------------------ |
//! | `root`     | The commit point: the directory of files and records, and what each names |
//! | `blob/<n>` | One zstd frame, written once and never changed: a chunk, or a record     |
//!
//! A paged file is a row of *chunks* of [`StateOptions::chunk_pages`] pages
//! each. A chunk is stored as a *base*, a frame of its pages compressed
//! against its file's [`Base`] pages (a VM's golden image) if it has one, and,
//! maybe, a *delta*: a frame of its pages now, compressed against the base,
//! so it costs about what changed since the base. A chunk with neither is
//! its file's base pages, or zeros. A flush writes each chunk it changed as a
//! new delta against the chunk's base, or, once that delta passes
//! [`StateOptions::rebase_ratio`] of the base's size, as a new base. A record of more than
//! [`StateOptions::inline_record_bytes`] is a blob of its own; smaller ones
//! are in the root.
//!
//! **One rule.** A blob is written before the first root that names it, and
//! deleted only once no root a reader may hold names it. The store keeps
//! writes in issue order, so a root is stored only with the blobs it names:
//! whatever prefix of a flush's writes a crash keeps, the root stored names
//! only blobs that are there. There is no log to replay, nothing is
//! overwritten, and nothing needs repair after a crash.
//!
//! **Flush.** Delete the blobs that died [`StateOptions::garbage_after`]
//! flushes ago or earlier; put the flush's blobs, at numbers no durable root
//! has used; put the root, at the next version, naming them, and listing the
//! blobs it no longer names as garbage, with its version; sync once.
//!
//! **Open.** Get the root, and the blobs of its large records (and, cold, of a
//! small file), in one more round trip. A page read gets its chunk's base and
//! delta together, in one.
//!
//! **Readers and caching.** A blob's content never changes once a durable root
//! names it, so any state may cache it under its number forever, and a
//! state reads the root it opened as a consistent snapshot while the owner
//! flushes, for [`StateOptions::garbage_after`] of the owner's flushes. (A
//! crashed flush's blobs, which no durable root names, are overwritten by
//! the next flush: nothing caches them, since nothing caches a blob before a
//! durable root names it.)
use crate::{Base, PAGE, Page, StateOptions, cache::NamespaceCache, kv::KvStore, zstd};
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt, Shared, WeakShared, try_join_all};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex},
};

pub(crate) const ROOT: &str = "root";
pub(crate) const BLOB_PREFIX: &str = "blob/";
const ROOT_MAGIC: &[u8; 4] = b"SR16";
/// Bytes of a record's value.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of a record's or file's name.
pub const MAX_NAME_BYTES: usize = 255;
/// Bytes of a paged file: 64 MiB, a VM's largest memory. The root maps every
/// chunk of every file, so a flush rewrites a few bytes per stored chunk.
pub const MAX_FILE_BYTES: u64 = 64 << 20;
/// Files and records a root may name.
const MAX_FILES: u32 = 1 << 16;
/// Largest version or blob number a root may hold, far from overflow.
const MAX_SEQUENCE: u64 = 1 << 62;
/// Largest decoded root.
const MAX_ROOT_BYTES: usize = 64 << 20;
/// Chunk images a state keeps decoded before it drops them all.
const KEPT_IMAGES: usize = 1024;
/// Consecutive chunk misses that start sequential readahead.
const READAHEAD_AFTER: u32 = 2;
/// Most chunks read ahead at once.
const MAX_READAHEAD: u64 = 16;

pub(crate) fn blob_key(n: u64) -> String {
    format!("{BLOB_PREFIX}{n:x}")
}

/// The blob number a key names.
#[cfg(feature = "simulation")]
pub(crate) fn parse_blob_key(key: &str) -> Option<u64> {
    u64::from_str_radix(key.strip_prefix(BLOB_PREFIX)?, 16).ok()
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

pub(crate) fn get_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        n |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return (shift < 63 || byte <= 1).then_some(n);
        }
    }
    None
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
}

fn refuse<T>(message: String) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// The records' values, by name.
pub(crate) type Records = BTreeMap<String, Bytes>;

/// Logical page replacements of one file, by page number; `None` is a hole.
pub(crate) type Changes = BTreeMap<u64, Option<Page>>;

/// What a WAL writes to one paged file: the pages it changed, and its size:
/// set ([`crate::Wal::set_len`]), or else only what the pages written need
/// (the file's length when the WAL first wrote it, grown by its writes),
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

/// What a flush wrote.
#[derive(Clone, Debug, Default)]
pub struct FlushReport {
    /// Bytes of the blobs it put: its chunks' deltas and bases, and its large
    /// records.
    pub blob_bytes: usize,
    /// Bytes of the root it put; 0 when nothing changed.
    pub root_bytes: usize,
    /// Chunks it wrote as a new base rather than a delta.
    pub rebased: usize,
}

/// What a state opened with: its options, and its namespace in the
/// process's [`crate::PageCache`].
#[derive(Clone)]
pub(crate) struct Settings {
    pub options: StateOptions,
    pub cache: Option<NamespaceCache>,
}

impl std::ops::Deref for Settings {
    type Target = StateOptions;
    fn deref(&self) -> &StateOptions {
        &self.options
    }
}

/// Where a chunk is stored: its base, and its delta against that base; a
/// chunk with neither is its file's base pages, or zeros.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub base: Option<u64>,
    pub delta: Option<u64>,
}

impl Chunk {
    /// The blob holding the chunk's pages now.
    fn current(self) -> Option<u64> {
        self.delta.or(self.base)
    }

    pub(crate) fn blobs(self) -> impl Iterator<Item = u64> {
        self.base.into_iter().chain(self.delta)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Inline(Bytes),
    Blob(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Record(Value),
    /// Its size, its base's id ([`Base::id`]), and its chunks: as many as
    /// its size needs.
    Paged {
        size: u64,
        base: Option<u64>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub name: String,
    pub kind: Kind,
}

/// The root: the commit point.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Root {
    /// The flushes that changed something, ever.
    pub version: u64,
    /// The next blob number: none at or past it is named.
    pub next_blob: u64,
    /// Pages per chunk, set when the namespace was made.
    pub chunk_pages: u64,
    /// Blobs no root names since the version given, to delete.
    pub garbage: BTreeMap<u64, u64>,
    pub next_file: u32,
    pub directory: BTreeMap<u32, Entry>,
}

/// Reads varints and byte strings from a decoded root, failing on any
/// overrun.
struct Reading<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reading<'_> {
    fn varint(&mut self) -> io::Result<u64> {
        get_varint(self.bytes, &mut self.at).ok_or_else(|| invalid("root"))
    }

    /// A varint of at most `max`.
    fn bounded(&mut self, max: u64) -> io::Result<u64> {
        Some(self.varint()?)
            .filter(|&n| n <= max)
            .ok_or_else(|| invalid("root"))
    }

    fn bytes(&mut self) -> io::Result<&[u8]> {
        let len = self.varint()? as usize;
        let end = (self.at.checked_add(len))
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| invalid("root"))?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }
}

/// Blob numbers as deltas from the previous one of their stream, zigzagged,
/// plus one: 0 is none. A row of chunks written together costs a byte
/// each, and zstd folds runs of them.
fn put_ref(out: &mut Vec<u8>, previous: &mut u64, n: Option<u64>) {
    let Some(n) = n else {
        return out.push(0);
    };
    let delta = n.wrapping_sub(*previous) as i64;
    put_varint(out, ((delta << 1) ^ (delta >> 63)) as u64 + 1);
    *previous = n;
}

fn get_ref(r: &mut Reading, previous: &mut u64) -> io::Result<Option<u64>> {
    let z = r.varint()?;
    if z == 0 {
        return Ok(None);
    }
    let delta = ((z - 1) >> 1) as i64 ^ -(((z - 1) & 1) as i64);
    *previous = previous.wrapping_add(delta as u64);
    Ok(Some(*previous))
}

impl Root {
    fn new(chunk_pages: u64) -> Self {
        Self {
            version: 0,
            next_blob: 0,
            chunk_pages,
            garbage: BTreeMap::new(),
            next_file: 0,
            directory: BTreeMap::new(),
        }
    }

    pub fn file_id(&self, name: &str) -> Option<u32> {
        (self.directory.iter())
            .find(|(_, entry)| entry.name == name)
            .map(|(&id, _)| id)
    }

    fn chunk_bytes(&self) -> usize {
        self.chunk_pages as usize * PAGE as usize
    }

    /// Chunks of a file of `size` bytes.
    fn chunks_of(&self, size: u64) -> usize {
        size.div_ceil(self.chunk_pages * PAGE) as usize
    }

    /// Every blob it names.
    pub fn named(&self) -> BTreeSet<u64> {
        let mut named = BTreeSet::new();
        for entry in self.directory.values() {
            match &entry.kind {
                Kind::Record(Value::Blob(n)) => {
                    named.insert(*n);
                }
                Kind::Record(Value::Inline(_)) => {}
                Kind::Paged { chunks, .. } => named.extend(chunks.iter().flat_map(|c| c.blobs())),
            }
        }
        named
    }

    pub fn encode(&self) -> Bytes {
        let mut body = Vec::new();
        put_varint(&mut body, self.version);
        put_varint(&mut body, self.next_blob);
        put_varint(&mut body, self.chunk_pages);
        put_varint(&mut body, self.garbage.len() as u64);
        let mut previous = 0;
        for (&n, &since) in &self.garbage {
            put_varint(&mut body, n - previous);
            put_varint(&mut body, self.version - since);
            previous = n;
        }
        put_varint(&mut body, u64::from(self.next_file));
        put_varint(&mut body, self.directory.len() as u64);
        let mut next = 0;
        for (&id, entry) in &self.directory {
            put_varint(&mut body, u64::from(id - next));
            next = id + 1;
            put_varint(&mut body, entry.name.len() as u64);
            body.extend_from_slice(entry.name.as_bytes());
            match &entry.kind {
                Kind::Record(Value::Inline(bytes)) => {
                    body.push(0);
                    put_varint(&mut body, bytes.len() as u64);
                    body.extend_from_slice(bytes);
                }
                Kind::Record(Value::Blob(n)) => {
                    body.push(1);
                    put_varint(&mut body, *n);
                }
                Kind::Paged { size, base, chunks } => {
                    body.push(2);
                    put_varint(&mut body, size / 512);
                    put_varint(&mut body, base.map_or(0, |id| id.saturating_add(1)));
                    let (mut bases, mut deltas) = (0, 0);
                    for chunk in chunks {
                        put_ref(&mut body, &mut bases, chunk.base);
                        put_ref(&mut body, &mut deltas, chunk.delta);
                    }
                }
            }
        }
        let mut out = ROOT_MAGIC.to_vec();
        put_varint(&mut out, body.len() as u64);
        zstd::compress(&body, &[], false, |frame| out.extend_from_slice(frame))
            .expect("compressing in memory");
        Bytes::from(out)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let rest = (bytes.strip_prefix(ROOT_MAGIC)).ok_or_else(|| invalid("root"))?;
        let mut at = 0;
        let len = get_varint(rest, &mut at).ok_or_else(|| invalid("root"))? as usize;
        if len > MAX_ROOT_BYTES {
            return Err(invalid("root"));
        }
        let mut body = Vec::with_capacity(len);
        zstd::decompress(&mut body, &rest[at..], &[])?;
        if body.len() != len {
            return Err(invalid("root"));
        }
        let mut r = Reading {
            bytes: &body,
            at: 0,
        };
        let version = r.bounded(MAX_SEQUENCE)?;
        let next_blob = r.bounded(MAX_SEQUENCE)?;
        let chunk_pages = r.bounded(MAX_FILE_BYTES / PAGE)?.max(1);
        let mut root = Root {
            version,
            next_blob,
            ..Root::new(chunk_pages)
        };
        let blob = |n: u64| {
            Some(n)
                .filter(|&n| n < next_blob)
                .ok_or_else(|| invalid("root"))
        };
        let mut previous = 0u64;
        for _ in 0..r.bounded(next_blob)? {
            previous = blob(
                previous
                    .checked_add(r.varint()?)
                    .ok_or_else(|| invalid("root"))?,
            )?;
            let since = version
                .checked_sub(r.varint()?)
                .ok_or_else(|| invalid("root"))?;
            root.garbage.insert(previous, since);
        }
        root.next_file = r.bounded(MAX_FILES.into())? as u32;
        let mut next = 0u32;
        for _ in 0..r.bounded(root.next_file.into())? {
            let id = next
                .checked_add(r.bounded(MAX_FILES.into())? as u32)
                .filter(|&id| id < root.next_file)
                .ok_or_else(|| invalid("root"))?;
            next = id + 1;
            let name = String::from_utf8(r.bytes()?.to_vec()).map_err(|_| invalid("root"))?;
            if name.len() > MAX_NAME_BYTES {
                return Err(invalid("root"));
            }
            let kind = match r.varint()? {
                0 => Kind::Record(Value::Inline(Bytes::copy_from_slice(r.bytes()?))),
                1 => Kind::Record(Value::Blob(blob(r.varint()?)?)),
                2 => {
                    let size = r.bounded(MAX_FILE_BYTES / 512)? * 512;
                    let base = r.varint()?.checked_sub(1);
                    let (mut bases, mut deltas) = (0, 0);
                    let chunks = (0..root.chunks_of(size))
                        .map(|_| {
                            let base = get_ref(&mut r, &mut bases)?.map(blob).transpose()?;
                            let delta = get_ref(&mut r, &mut deltas)?.map(blob).transpose()?;
                            Ok(Chunk { base, delta })
                        })
                        .collect::<io::Result<_>>()?;
                    Kind::Paged { size, base, chunks }
                }
                _ => return Err(invalid("root")),
            };
            root.directory.insert(id, Entry { name, kind });
        }
        if r.at != body.len() {
            return Err(invalid("root"));
        }
        Ok(root)
    }
}

/// What chunk `chunk` holds without a stored base: its file's `base` pages,
/// or zeros.
fn implicit(base: Option<&Arc<dyn Base>>, chunk_pages: u64, chunk: u64) -> Vec<u8> {
    let mut pages = vec![0; (chunk_pages * PAGE) as usize];
    if let Some(base) = base {
        for (i, page) in pages.chunks_mut(PAGE as usize).enumerate() {
            if let Some(image) = base.page(chunk * chunk_pages + i as u64) {
                page.copy_from_slice(image);
            }
        }
    }
    pages
}

/// What a chunk's base blob is compressed against: its file's `base` pages
/// (zeros past them), or nothing for a file without one.
fn reference(base: Option<&Arc<dyn Base>>, chunk_pages: u64, chunk: u64) -> Vec<u8> {
    match base {
        Some(_) => implicit(base, chunk_pages, chunk),
        None => Vec::new(),
    }
}

/// A decoded chunk: its pages, and the bytes of the blob that held them.
pub(crate) struct Image {
    pages: Bytes,
    stored: usize,
}

/// A blob being fetched, which concurrent reads of it share.
type Fetch = Shared<BoxFuture<'static, Result<Bytes, Arc<io::Error>>>>;
/// A fetch held only by its readers: when all of them are dropped, so is
/// the request.
type Fetching = HashMap<u64, WeakShared<BoxFuture<'static, Result<Bytes, Arc<io::Error>>>>>;

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: Settings,
    root: Arc<Root>,
    /// The root's exact bytes (`None` when absent).
    root_bytes: Option<Bytes>,
    records: Records,
    /// The bases files were opened with, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
    /// Nothing changed since this owner last saw the namespace.
    pub(crate) unchanged: bool,
    /// A reader's: it changes nothing in the cache.
    reader: bool,
    /// Chunk images decoded, by the blob that holds them: blobs never change,
    /// so every view of the state shares them.
    images: Arc<Mutex<HashMap<u64, Arc<Image>>>>,
    fetching: Arc<Mutex<Fetching>>,
    readahead: Mutex<Readahead>,
}

/// Detects sequential chunk misses.
#[derive(Default)]
struct Readahead {
    last: Option<(u32, u64)>,
    run: u32,
    window: u64,
}

impl Readahead {
    /// Chunks to fetch after a miss of chunk `chunk` of file `file`.
    fn miss(&mut self, file: u32, chunk: u64) -> u64 {
        self.run = match self.last {
            Some(last) if last == (file, chunk.wrapping_sub(1)) => self.run + 1,
            _ => 1,
        };
        self.last = Some((file, chunk));
        self.window = match self.run >= READAHEAD_AFTER {
            true => (self.window * 2).clamp(2, MAX_READAHEAD),
            false => 0,
        };
        self.window
    }
}

impl Snapshot {
    /// Open the namespace: see the module documentation. An owner without a
    /// cache, or whose last flush had an unknown outcome, syncs alongside, so
    /// that what it reads is durable; a `reader` reads what is stored.
    pub async fn open(
        store: Arc<dyn KvStore>,
        options: Settings,
        reader: bool,
    ) -> io::Result<Self> {
        let cache = options.cache.as_ref().filter(|_| !reader);
        let unsettled = cache.and_then(NamespaceCache::unsettled);
        let settle = !reader && (cache.is_none() || unsettled.is_some());
        let settle = async {
            match settle {
                true => store.sync().await,
                false => Ok(()),
            }
        };
        let (root_bytes, ()) = futures::try_join!(store.get(ROOT), settle)?;
        if let (Some(cache), Some(unknown)) = (cache, unsettled) {
            cache.settled(unknown);
        }
        let root = match &root_bytes {
            Some(bytes) => Root::decode(bytes)?,
            None => Root::new(options.chunk_pages.clamp(1, MAX_FILE_BYTES / PAGE)),
        };
        let mut snapshot = Self {
            store,
            options: options.clone(),
            root: Arc::new(root),
            root_bytes,
            records: Records::new(),
            bases: BTreeMap::new(),
            unchanged: false,
            reader,
            images: Arc::default(),
            fetching: Arc::default(),
            readahead: Mutex::default(),
        };
        // The large records' blobs, and every chunk of a small file, together.
        let mut wanted = BTreeSet::new();
        for entry in snapshot.root.directory.values() {
            match &entry.kind {
                Kind::Record(Value::Blob(n)) => {
                    wanted.insert(*n);
                }
                Kind::Paged {
                    size,
                    base: None,
                    chunks,
                } if size.div_ceil(PAGE) <= options.prefetch_pages => {
                    wanted.extend(chunks.iter().flat_map(|c| c.blobs()))
                }
                _ => {}
            }
        }
        let wanted: Vec<u64> = wanted.into_iter().collect();
        let fetched: HashMap<u64, Bytes> = wanted
            .iter()
            .copied()
            .zip(snapshot.blobs(&wanted).await?)
            .collect();
        for (&id, entry) in &snapshot.root.directory {
            if let Kind::Paged {
                size,
                base: None,
                chunks,
            } = &entry.kind
                && size.div_ceil(PAGE) <= options.prefetch_pages
            {
                for (chunk, stored) in chunks.iter().enumerate() {
                    if stored.current().is_some() {
                        snapshot.decode(id, chunk as u64, &fetched)?;
                    }
                }
            }
        }
        for entry in snapshot.root.directory.values() {
            if let Kind::Record(value) = &entry.kind {
                let value = match value {
                    Value::Inline(bytes) => bytes.clone(),
                    Value::Blob(n) => {
                        Bytes::from(zstd::decompress_sized(&fetched[n], &[], MAX_RECORD_BYTES)?)
                    }
                };
                snapshot.records.insert(entry.name.clone(), value);
            }
        }
        if let Some(cache) = cache {
            let root = &snapshot.root;
            let known = cache.validate(snapshot.root_bytes.as_deref());
            snapshot.unchanged = known && cache.committed() == Some(root.version);
            cache.record(
                snapshot.root_bytes.clone(),
                (!known).then_some(root.version),
            );
        }
        Ok(snapshot)
    }

    /// Always: a state reads the root it opened, whose blobs never change. A
    /// read of one the owner deleted since fails instead.
    pub(crate) fn is_current(&self) -> bool {
        true
    }

    pub fn version(&self) -> u64 {
        self.root.version
    }

    pub fn records(&self) -> &Records {
        &self.records
    }

    pub fn file_size(&self, name: &str) -> u64 {
        match self
            .root
            .file_id(name)
            .map(|id| &self.root.directory[&id].kind)
        {
            Some(Kind::Paged { size, .. }) => *size,
            _ => 0,
        }
    }

    pub(crate) fn is_paged(&self, name: &str) -> Option<bool> {
        let id = self.root.file_id(name)?;
        Some(matches!(self.root.directory[&id].kind, Kind::Paged { .. }))
    }

    /// The base the file `id` was opened with: `Err` for a file made with one
    /// that it was not opened with.
    fn base_of(&self, id: u32) -> io::Result<Option<&Arc<dyn Base>>> {
        let entry = &self.root.directory[&id];
        match (&entry.kind, self.bases.get(&entry.name)) {
            (Kind::Paged { base: Some(_), .. }, None) => Err(io::Error::other(format!(
                "{:?} has a base: open it with it",
                entry.name
            ))),
            (_, base) => Ok(base),
        }
    }

    fn chunks(&self, id: u32) -> &[Chunk] {
        match &self.root.directory[&id].kind {
            Kind::Paged { chunks, .. } => chunks,
            Kind::Record(_) => &[],
        }
    }

    /// Page `page` of the file `name`; `None` for zeros (a hole of a file
    /// without a base, or past its end).
    pub async fn read(&self, name: &str, page: u64) -> io::Result<Option<Page>> {
        let Some(id) = self.root.file_id(name) else {
            return Ok(None);
        };
        let (chunk, at) = (page / self.root.chunk_pages, page % self.root.chunk_pages);
        let at = at as usize * PAGE as usize;
        match self.chunks(id).get(chunk as usize) {
            Some(stored) if stored.current().is_some() => {
                let image = self.image(id, chunk).await?;
                let image = image.pages.slice(at..at + PAGE as usize);
                // Zeros where the base has no page read as a hole.
                let based = self
                    .base_of(id)?
                    .is_some_and(|base| base.page(page).is_some());
                Ok((based || image.iter().any(|&byte| byte != 0)).then_some(image))
            }
            None => Ok(None),
            Some(_) => Ok(self
                .base_of(id)?
                .and_then(|base| base.page(page))
                .map(Page::copy_from_slice)),
        }
    }

    /// Chunk `chunk` of the file `id`, which a blob holds: decoded, or
    /// fetched with its base, and with the chunks after it when reads are
    /// sequential, in one round trip.
    async fn image(&self, id: u32, chunk: u64) -> io::Result<Arc<Image>> {
        let chunks = self.chunks(id);
        let current = chunks[chunk as usize].current().expect("a stored chunk");
        if let Some(image) = self.images.lock().unwrap().get(&current) {
            return Ok(image.clone());
        }
        let ahead = self.readahead.lock().unwrap().miss(id, chunk);
        let wanted: Vec<u64> = (chunk..(chunk + 1 + ahead).min(chunks.len() as u64))
            .filter(|&c| c == chunk || !self.has(chunks[c as usize]))
            .collect();
        let blobs: Vec<u64> = (wanted.iter())
            .flat_map(|&c| chunks[c as usize].blobs())
            .collect();
        let fetched: HashMap<u64, Bytes> = blobs
            .iter()
            .copied()
            .zip(self.blobs(&blobs).await?)
            .collect();
        for &c in &wanted {
            self.decode(id, c, &fetched)?;
        }
        let image = self.images.lock().unwrap().get(&current).cloned();
        image.ok_or_else(|| io::Error::other("a decoded chunk was dropped"))
    }

    /// The base blob `n` of a chunk, decoded.
    async fn base_image(&self, id: u32, chunk: u64, n: u64) -> io::Result<Arc<Image>> {
        if let Some(image) = self.images.lock().unwrap().get(&n) {
            return Ok(image.clone());
        }
        let frame = self.blob(n).await?;
        let prefix = reference(self.base_of(id)?, self.root.chunk_pages, chunk);
        let mut pages = Vec::with_capacity(self.root.chunk_bytes());
        zstd::decompress(&mut pages, &frame, &prefix)?;
        if pages.len() != self.root.chunk_bytes() {
            return Err(invalid("chunk"));
        }
        Ok(Arc::new(Image {
            pages: pages.into(),
            stored: frame.len(),
        }))
    }

    fn has(&self, chunk: Chunk) -> bool {
        chunk
            .current()
            .is_none_or(|n| self.images.lock().unwrap().contains_key(&n))
    }

    /// Decode chunk `chunk` of the file `id` from its blobs in `fetched`, and
    /// keep its base and its pages now.
    fn decode(&self, id: u32, chunk: u64, fetched: &HashMap<u64, Bytes>) -> io::Result<()> {
        let stored = self.chunks(id)[chunk as usize];
        let size = self.root.chunk_bytes();
        let decode = |n: u64, prefix: &[u8]| -> io::Result<Arc<Image>> {
            let frame = &fetched[&n];
            let mut pages = Vec::with_capacity(size);
            zstd::decompress(&mut pages, frame, prefix)?;
            if pages.len() != size {
                return Err(invalid("chunk"));
            }
            Ok(Arc::new(Image {
                pages: pages.into(),
                stored: frame.len(),
            }))
        };
        let base = match stored.base {
            Some(n) => {
                let prefix = reference(self.base_of(id)?, self.root.chunk_pages, chunk);
                Some((n, decode(n, &prefix)?))
            }
            None => None,
        };
        let delta = match (stored.delta, &base) {
            (Some(n), Some((_, base))) => Some((n, decode(n, &base.pages)?)),
            (Some(_), None) => return Err(invalid("chunk: a delta without a base")),
            (None, _) => None,
        };
        let mut images = self.images.lock().unwrap();
        if images.len() >= KEPT_IMAGES {
            images.clear();
        }
        images.extend(base.into_iter().chain(delta));
        Ok(())
    }

    /// Blobs `wanted`, in order: from the cache, or the store, together.
    async fn blobs(&self, wanted: &[u64]) -> io::Result<Vec<Bytes>> {
        try_join_all(wanted.iter().map(|&n| self.blob(n))).await
    }

    async fn blob(&self, n: u64) -> io::Result<Bytes> {
        let cache = self.options.cache.clone();
        if let Some(cache) = &cache
            && let Some(value) = cache.get(n).await
        {
            return Ok(value);
        }
        // A reader's root may be a write not yet durable (as the store shows
        // it), whose blobs a crash could lose and the next flush reuse: only
        // the owner, whose roots are durable, caches what it fetches.
        let cache = cache.filter(|_| !self.reader);
        let fetch = {
            let mut fetching = self.fetching.lock().unwrap();
            if let Some(fetch) = fetching.get(&n).and_then(WeakShared::upgrade) {
                fetch
            } else {
                let store = self.store.clone();
                let fetch: Fetch = async move {
                    let value = store.get(&blob_key(n)).await.map_err(Arc::new)?;
                    let value = value.ok_or_else(|| {
                        Arc::new(io::Error::other(format!(
                            "blob {n} is gone: the owner deleted it since this state opened"
                        )))
                    })?;
                    // A durable root names it: it never changes now.
                    if let Some(cache) = cache {
                        cache.insert(n, value.clone());
                    }
                    Ok(value)
                }
                .boxed()
                .shared();
                fetching.insert(n, fetch.downgrade().expect("not polled yet"));
                fetch
            }
        };
        let value = fetch.await;
        self.fetching.lock().unwrap().remove(&n);
        value.map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }

    /// Refuse what no flush could store, before anything is written.
    pub(crate) fn check(&self, writes: &Writes, records: &Records) -> io::Result<()> {
        let paged = |name: &str| self.is_paged(name);
        for (name, value) in records {
            if value.len() > MAX_RECORD_BYTES {
                let (len, max) = (value.len(), MAX_RECORD_BYTES);
                return refuse(format!("record {name:?} of {len} bytes exceeds {max}"));
            }
            if paged(name) == Some(true) {
                return refuse(format!("{name:?} is a paged file, not a record"));
            }
        }
        for name in records.keys().chain(writes.keys()) {
            if name.len() > MAX_NAME_BYTES {
                return refuse(format!("name {name:?} exceeds {MAX_NAME_BYTES} bytes"));
            }
        }
        for (name, written) in writes {
            if records.contains_key(name) || paged(name) == Some(false) {
                return refuse(format!("{name:?} is a record, not a paged file"));
            }
            let size = written.size_after(self.file_size(name));
            if !size.is_multiple_of(512) || size > MAX_FILE_BYTES {
                return refuse(format!("{name:?}: invalid size {size}"));
            }
            if written
                .pages
                .keys()
                .any(|&page| page >= MAX_FILE_BYTES / PAGE)
            {
                return refuse(format!("{name:?}: a page past the largest file"));
            }
        }
        Ok(())
    }

    /// Flush `writes` and `records`: see the module documentation.
    pub(crate) async fn commit(
        &self,
        mut writes: Writes,
        mut records: Records,
    ) -> io::Result<(FlushReport, Self)> {
        self.check(&writes, &records)?;
        records.retain(|name, value| {
            (self.records.get(name)).map_or(!value.is_empty(), |held| held != value)
        });
        writes.retain(|name, written| {
            let current = self.file_size(name);
            !written.pages.is_empty() || written.size_after(current) != current
        });
        if writes.is_empty() && records.is_empty() {
            return Ok((
                FlushReport::default(),
                self.continued(self.root.clone(), self.root_bytes.clone()),
            ));
        }
        let cache = self.options.cache.as_ref();
        let mut root = (*self.root).clone();
        root.version += 1;
        let version = root.version;
        let mut fresh: Vec<(u64, Bytes, Option<Arc<Image>>)> = Vec::new();
        let mut dead = Vec::new();
        let mut rebased = 0;
        let allocate = |root: &mut Root, name: &str, kind: Kind| match root.file_id(name) {
            Some(id) => Ok(id),
            None if root.next_file >= MAX_FILES => refuse("too many files".into()),
            None => {
                let id = root.next_file;
                root.next_file += 1;
                root.directory.insert(
                    id,
                    Entry {
                        name: name.into(),
                        kind,
                    },
                );
                Ok(id)
            }
        };
        for (name, written) in writes {
            let base = self.bases.get(&name);
            let empty = Kind::Paged {
                size: 0,
                base: base.map(|base| base.id()),
                chunks: Vec::new(),
            };
            let id = allocate(&mut root, &name, empty)?;
            let current = self.file_size(&name);
            let size = written.size_after(current);
            let held = self
                .root
                .directory
                .get(&id)
                .map_or(&[][..], |_| self.chunks(id));
            let based = matches!(&root.directory[&id].kind, Kind::Paged { base: Some(_), .. });
            if based && base.is_none() && written.pages.values().any(Option::is_some) {
                return refuse(format!("{name:?} has a base: open it with it"));
            }
            // The chunks it changes: those it writes, and the last one kept
            // when it shrinks (pages past the end become holes).
            let count = root.chunks_of(size);
            let mut touched: BTreeSet<u64> = (written.pages.keys())
                .map(|page| page / root.chunk_pages)
                .filter(|&chunk| chunk < count as u64)
                .collect();
            if size < current && size % (root.chunk_pages * PAGE) != 0 {
                touched.insert(size / (root.chunk_pages * PAGE));
            }
            let mut chunks: Vec<Chunk> = held.to_vec();
            for stored in chunks.drain(count.min(chunks.len())..) {
                dead.extend(stored.blobs());
            }
            chunks.resize(count, Chunk::default());
            for chunk in touched {
                let stored = chunks[chunk as usize];
                // Without its base, a file made with one can only drop
                // chunks: refused below if a page would stay.
                let missing = based && base.is_none();
                let hole = implicit(base, root.chunk_pages, chunk);
                let (base_image, now) = match stored.current() {
                    Some(_) if !missing => {
                        let base_image = match stored.base {
                            Some(n) => Some(self.base_image(id, chunk, n).await?),
                            None => None,
                        };
                        (base_image, self.image(id, chunk).await?.pages.to_vec())
                    }
                    _ => (None, hole.clone()),
                };
                let mut pages = now.clone();
                let first = chunk * root.chunk_pages;
                let mut kept = false;
                for (i, page) in pages.chunks_mut(PAGE as usize).enumerate() {
                    let index = first + i as u64;
                    let hole = &hole[i * PAGE as usize..(i + 1) * PAGE as usize];
                    match written.pages.get(&index) {
                        _ if index * PAGE >= size => page.copy_from_slice(hole),
                        Some(image) => page.copy_from_slice(image.as_deref().unwrap_or(hole)),
                        None => kept |= stored.current().is_some(),
                    }
                }
                if missing && kept {
                    return refuse(format!("{name:?} has a base: open it with it"));
                }
                if pages == now && stored.current().is_some() {
                    continue;
                }
                dead.extend(stored.delta);
                // Holes only: nothing stored.
                if pages == hole || missing {
                    dead.extend(stored.base);
                    chunks[chunk as usize] = Chunk::default();
                    continue;
                }
                // A delta against the stored base while it stays small
                // beside it; else a new base.
                let delta = match &base_image {
                    Some(base) => {
                        let delta =
                            zstd::compress(&pages, &base.pages, true, Bytes::copy_from_slice)?;
                        let small =
                            delta.len() as f64 <= self.options.rebase_ratio * base.stored as f64;
                        small.then_some(delta)
                    }
                    None => None,
                };
                let n = root.next_blob;
                root.next_blob += 1;
                let pages = Bytes::from(pages);
                chunks[chunk as usize] = match delta {
                    Some(delta) => {
                        let image = Image {
                            pages,
                            stored: delta.len(),
                        };
                        fresh.push((n, delta, Some(Arc::new(image))));
                        Chunk {
                            base: stored.base,
                            delta: Some(n),
                        }
                    }
                    None => {
                        let prefix = if based { &hole[..] } else { &[] };
                        let frame = zstd::compress(&pages, prefix, true, Bytes::copy_from_slice)?;
                        rebased += usize::from(stored.base.is_some());
                        dead.extend(stored.base);
                        let image = Image {
                            pages,
                            stored: frame.len(),
                        };
                        fresh.push((n, frame, Some(Arc::new(image))));
                        Chunk {
                            base: Some(n),
                            delta: None,
                        }
                    }
                };
            }
            let Some(Entry {
                kind:
                    Kind::Paged {
                        size: held_size,
                        chunks: held_chunks,
                        ..
                    },
                ..
            }) = root.directory.get_mut(&id)
            else {
                unreachable!("checked: a paged file");
            };
            *held_size = size;
            *held_chunks = chunks;
        }
        let mut after_records = self.records.clone();
        for (name, value) in records {
            let id = allocate(&mut root, &name, Kind::Record(Value::Inline(Bytes::new())))?;
            if let Kind::Record(Value::Blob(n)) = root.directory[&id].kind {
                dead.push(n);
            }
            let stored = match value.len() <= self.options.inline_record_bytes {
                true => Value::Inline(value.clone()),
                false => {
                    let n = root.next_blob;
                    root.next_blob += 1;
                    fresh.push((
                        n,
                        zstd::compress(&value, &[], true, Bytes::copy_from_slice)?,
                        None,
                    ));
                    Value::Blob(n)
                }
            };
            root.directory.get_mut(&id).expect("allocated").kind = Kind::Record(stored);
            after_records.insert(name, value);
        }
        if cache.is_some_and(|cache| !cache.claim(self.version())) {
            return refuse("another state flushed since this one opened; reopen".into());
        }
        // 1. The garbage old enough that no reader holds a root naming it.
        let grace = self.options.garbage_after.max(1);
        let (old, kept): (Vec<_>, Vec<_>) =
            (root.garbage.iter()).partition(|&(_, &since)| since + grace <= version);
        let old: Vec<u64> = old.into_iter().map(|(&n, _)| n).collect();
        root.garbage = kept.into_iter().map(|(&n, &since)| (n, since)).collect();
        root.garbage.extend(dead.into_iter().map(|n| (n, version)));
        let write = |key: &str, value: Option<Bytes>| {
            let queued = match value {
                Some(value) => self.store.put(key, value),
                None => self.store.delete(key),
            };
            queued.map_err(|error| self.failed(error))
        };
        for n in old {
            write(&blob_key(n), None)?;
        }
        // 2. The blobs, then 3. the root that names them; then one sync.
        let mut blob_bytes = 0;
        for (n, value, _) in &fresh {
            blob_bytes += value.len();
            write(&blob_key(*n), Some(value.clone()))?;
        }
        let bytes = root.encode();
        let root_bytes = bytes.len();
        write(ROOT, Some(bytes.clone()))?;
        if let Err(error) = self.store.sync().await {
            return Err(self.failed(error));
        }
        if let Some(cache) = cache {
            for (n, value, _) in &fresh {
                cache.insert(*n, value.clone());
            }
            cache.record(Some(bytes.clone()), Some(version));
        }
        let mut after = self.continued(Arc::new(root), Some(bytes));
        after.records = after_records;
        let mut images = after.images.lock().unwrap();
        for (n, _, image) in fresh {
            if let Some(image) = image {
                images.insert(n, image);
            }
        }
        drop(images);
        let report = FlushReport {
            blob_bytes,
            root_bytes,
            rebased,
        };
        Ok((report, after))
    }

    fn failed(&self, error: io::Error) -> io::Error {
        if let Some(cache) = &self.options.cache {
            cache.forget();
        }
        error
    }

    fn continued(&self, root: Arc<Root>, root_bytes: Option<Bytes>) -> Self {
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            root,
            root_bytes,
            records: self.records.clone(),
            bases: self.bases.clone(),
            unchanged: false,
            reader: self.reader,
            images: self.images.clone(),
            fetching: self.fetching.clone(),
            readahead: Mutex::default(),
        }
    }

    pub(crate) fn with_base(&self, name: &str, base: Arc<dyn Base>) -> io::Result<Self> {
        if let Some(id) = self.root.file_id(name) {
            match &self.root.directory[&id].kind {
                Kind::Paged {
                    base: Some(held), ..
                } if *held == base.id() => {}
                _ => return refuse(format!("{name:?} was made with another base, or none")),
            }
        }
        let mut with = self.continued(self.root.clone(), self.root_bytes.clone());
        with.unchanged = self.unchanged;
        with.bases.insert(name.to_owned(), base);
        Ok(with)
    }
}

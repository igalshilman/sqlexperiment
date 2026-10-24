//! The stored state: immutable blobs, and one root that names them.
//!
//! | Key            | Holds                                                                     |
//! | -------------- | ------------------------------------------------------------------------- |
//! | `v16/root`     | The commit point: the directory of files and records, and what each names |
//! | `v16/blob/<n>` | One zstd frame, written once and never changed: a chunk, or a record      |
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
//! names it, so any state may cache it under its number (and its database's
//! random identity, which a namespace made again changes) forever, and a
//! state reads the root it opened as a consistent snapshot while the owner
//! flushes, for [`StateOptions::garbage_after`] of the owner's flushes. (A
//! crashed flush's blobs, which no durable root names, are overwritten by
//! the next flush: nothing caches them, since nothing caches a blob before a
//! durable root names it.)
use crate::{
    Base, PAGE, Page, StateOptions,
    cache::{self, NamespaceCache},
    kv::KvStore,
    map::{MAX_FANOUT, Map, Node, Nodes, Shape},
    zstd,
};
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt, Shared, WeakShared, try_join_all};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex},
};

pub(crate) const ROOT: &str = "v16/root";
pub(crate) const BLOB_PREFIX: &str = "v16/blob/";
const ROOT_MAGIC: &[u8; 4] = b"SR16";
/// Bytes of a record's value.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of a record's or file's name.
pub const MAX_NAME_BYTES: usize = 255;
/// Bytes of a paged file: 10 GiB.
pub const MAX_FILE_BYTES: u64 = 10 << 30;
/// Most pages a chunk holds: 1 MiB.
pub(crate) const MAX_CHUNK_PAGES: u64 = 256;
/// Files and records a root may name.
const MAX_FILES: u32 = 1 << 16;
/// Largest version or blob number a root may hold, far from overflow.
const MAX_SEQUENCE: u64 = 1 << 62;
/// Largest decoded root.
const MAX_ROOT_BYTES: usize = 64 << 20;
/// Chunk images a state keeps decoded before it drops them all.
const KEPT_IMAGES: usize = 1024;
/// Map nodes a state keeps decoded before it drops them all.
const KEPT_NODES: usize = 1024;
/// Most map nodes an open fetches, a level a round trip, so that a cold
/// read pays for the map at most once: 512 MiB of file at the default
/// fanout and chunk size.
const PREFETCH_MAP_NODES: u64 = 512;
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

pub(crate) fn invalid(what: &str) -> io::Error {
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
    /// Chunks whose base it replaced with a new one, rather than write a
    /// delta against it.
    pub rebased: usize,
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
    /// Its size, its base's id ([`Base::id`]), and the map of its chunks:
    /// as many as its size needs.
    Paged {
        size: u64,
        base: Option<u64>,
        map: Map,
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
    /// The database's identity, drawn at random when its first root was
    /// made: its blob numbers are unique under it, and the cache keys them
    /// by both (a namespace emptied and made again counts from 0 again).
    pub id: u64,
    /// The flushes that changed something, ever.
    pub version: u64,
    /// The next blob number: none at or past it is named.
    pub next_blob: u64,
    /// Pages per chunk, set when the namespace was made.
    pub chunk_pages: u64,
    /// Entries per node of a file's chunk map, set when the namespace was
    /// made.
    pub fanout: u64,
    /// Blobs no root names since the version given, to delete.
    pub garbage: BTreeMap<u64, u64>,
    pub next_file: u32,
    pub directory: BTreeMap<u32, Entry>,
}

/// Reads varints and byte strings from a decoded root or map node, failing
/// on any overrun.
pub(crate) struct Reading<'a> {
    bytes: &'a [u8],
    at: usize,
    /// What it reads, for errors.
    what: &'static str,
}

impl<'a> Reading<'a> {
    pub fn new(bytes: &'a [u8], what: &'static str) -> Self {
        Self { bytes, at: 0, what }
    }

    pub fn invalid(&self) -> io::Error {
        invalid(self.what)
    }

    pub fn varint(&mut self) -> io::Result<u64> {
        get_varint(self.bytes, &mut self.at).ok_or_else(|| self.invalid())
    }

    /// A varint of at most `max`.
    fn bounded(&mut self, max: u64) -> io::Result<u64> {
        Some(self.varint()?)
            .filter(|&n| n <= max)
            .ok_or_else(|| self.invalid())
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = (self.at.checked_add(len))
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| self.invalid())?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    /// Fail unless every byte was read.
    pub fn end(&self) -> io::Result<()> {
        match self.at == self.bytes.len() {
            true => Ok(()),
            false => Err(self.invalid()),
        }
    }
}

/// Blob numbers as deltas from the previous one of their stream, zigzagged,
/// plus one: 0 is none. A row of chunks written together costs a byte
/// each, and zstd folds runs of them.
pub(crate) fn put_ref(out: &mut Vec<u8>, previous: &mut u64, n: Option<u64>) {
    let Some(n) = n else {
        return out.push(0);
    };
    let delta = n.wrapping_sub(*previous) as i64;
    put_varint(out, ((delta << 1) ^ (delta >> 63)) as u64 + 1);
    *previous = n;
}

pub(crate) fn get_ref(r: &mut Reading, previous: &mut u64) -> io::Result<Option<u64>> {
    let z = r.varint()?;
    if z == 0 {
        return Ok(None);
    }
    let delta = ((z - 1) >> 1) as i64 ^ -(((z - 1) & 1) as i64);
    *previous = previous.wrapping_add(delta as u64);
    Ok(Some(*previous))
}

impl Root {
    fn new(chunk_pages: u64, fanout: u64) -> Self {
        Self {
            id: cache::random(),
            version: 0,
            next_blob: 0,
            chunk_pages,
            fanout,
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

    /// The shape of the map of a file of `size` bytes.
    pub fn shape(&self, size: u64) -> Shape {
        Shape {
            fanout: self.fanout,
            count: size.div_ceil(self.chunk_pages * PAGE),
        }
    }

    pub fn encode(&self) -> Bytes {
        let mut body = self.id.to_le_bytes().to_vec();
        put_varint(&mut body, self.version);
        put_varint(&mut body, self.next_blob);
        put_varint(&mut body, self.chunk_pages);
        put_varint(&mut body, self.fanout);
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
                Kind::Paged { size, base, map } => {
                    body.push(2);
                    put_varint(&mut body, size / 512);
                    put_varint(&mut body, base.map_or(0, |id| id + 1));
                    map.encode(&mut body);
                }
            }
        }
        let mut out = ROOT_MAGIC.to_vec();
        zstd::compress(&body, &[], |frame| out.extend_from_slice(frame))
            .expect("compressing in memory");
        Bytes::from(out)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let frame = (bytes.strip_prefix(ROOT_MAGIC)).ok_or_else(|| invalid("root"))?;
        let body = zstd::decompress_sized(frame, &[], MAX_ROOT_BYTES)?;
        let (id, rest) = body.split_first_chunk().ok_or_else(|| invalid("root"))?;
        let id = u64::from_le_bytes(*id);
        let mut r = Reading::new(rest, "root");
        let version = r.bounded(MAX_SEQUENCE)?;
        let next_blob = r.bounded(MAX_SEQUENCE)?;
        let chunk_pages = r.bounded(MAX_CHUNK_PAGES)?;
        let fanout = r.bounded(MAX_FANOUT)?;
        if chunk_pages == 0 || fanout < 2 {
            return Err(invalid("root"));
        }
        let mut root = Root {
            id,
            version,
            next_blob,
            ..Root::new(chunk_pages, fanout)
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
                    let map = Map::decode(&mut r, root.shape(size), next_blob)?;
                    Kind::Paged { size, base, map }
                }
                _ => return Err(invalid("root")),
            };
            root.directory.insert(id, Entry { name, kind });
        }
        r.end()?;
        Ok(root)
    }
}

/// A chunk's frame, decoded against `prefix`: exactly `size` bytes.
pub(crate) fn decode_chunk(frame: &[u8], prefix: &[u8], size: usize) -> io::Result<Arc<Image>> {
    let mut pages = Vec::with_capacity(size);
    zstd::decompress(&mut pages, frame, prefix)?;
    if pages.len() != size {
        return Err(invalid("chunk"));
    }
    Ok(Arc::new(Image {
        pages: pages.into(),
        stored: frame.len(),
    }))
}

/// The chunks a flush changes when it writes `written` into a file of
/// `current` bytes, with `per` pages a chunk: those it writes below its new
/// size, and the last one kept when it shrinks (pages past the end become
/// holes).
fn touched(written: &Written, current: u64, per: u64) -> impl Iterator<Item = u64> {
    let size = written.size_after(current);
    let mut touched: BTreeSet<u64> = (written.pages.keys())
        .map(|page| page / per)
        .filter(|&chunk| chunk * per * PAGE < size)
        .collect();
    if size < current && !size.is_multiple_of(per * PAGE) {
        touched.insert(size / (per * PAGE));
    }
    touched.into_iter()
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
pub(crate) fn reference(base: Option<&Arc<dyn Base>>, chunk_pages: u64, chunk: u64) -> Vec<u8> {
    match base {
        Some(_) => implicit(base, chunk_pages, chunk),
        None => Vec::new(),
    }
}

/// A decoded chunk: its pages, and the bytes of the blob that held them.
pub(crate) struct Image {
    pub pages: Bytes,
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
    options: StateOptions,
    /// The namespace in the process's [`crate::PageCache`].
    cache: Option<NamespaceCache>,
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
    /// Map nodes decoded, by blob, shared the same way.
    nodes: Arc<Mutex<HashMap<u64, Arc<Node>>>>,
    fetching: Arc<Mutex<Fetching>>,
    readahead: Mutex<Readahead>,
}

/// Detects sequential chunk misses.
#[derive(Default)]
struct Readahead {
    /// The file and last chunk the previous miss fetched.
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
        self.window = match self.run >= READAHEAD_AFTER {
            true => (self.window * 2).clamp(2, MAX_READAHEAD),
            false => 0,
        };
        self.last = Some((file, chunk + self.window));
        self.window
    }
}

/// A flush being built: the next root, and the blobs it puts and frees.
struct Flush {
    root: Root,
    /// Blobs to put, with the chunk images they hold.
    fresh: Vec<(u64, Bytes, Option<Arc<Image>>)>,
    /// Blobs the next root no longer names.
    dead: Vec<u64>,
    /// The map nodes it puts, decoded.
    nodes: Vec<(u64, Arc<Node>)>,
    /// Chunks given a new base in place of their old one.
    rebased: usize,
}

impl Flush {
    fn new(root: &Root) -> Self {
        let mut root = root.clone();
        root.version += 1;
        Self {
            root,
            fresh: Vec::new(),
            dead: Vec::new(),
            nodes: Vec::new(),
            rebased: 0,
        }
    }

    /// The id of the file `name`, made with `kind` if new (within
    /// [`MAX_FILES`]: checked).
    fn file(&mut self, name: &str, kind: impl FnOnce() -> Kind) -> u32 {
        if let Some(id) = self.root.file_id(name) {
            return id;
        }
        let id = self.root.next_file;
        self.root.next_file += 1;
        let entry = Entry {
            name: name.into(),
            kind: kind(),
        };
        self.root.directory.insert(id, entry);
        id
    }

    /// Put `frame`, which holds a chunk's `pages` if any, at the next blob
    /// number: the number.
    fn put(&mut self, frame: Bytes, pages: Option<Bytes>) -> u64 {
        let n = self.root.next_blob;
        self.root.next_blob += 1;
        let stored = frame.len();
        let image = pages.map(|pages| Arc::new(Image { pages, stored }));
        self.fresh.push((n, frame, image));
        n
    }

    /// Put map node `node`, which [`Map::update`] numbered `n`.
    fn put_node(&mut self, n: u64, node: Arc<Node>) -> io::Result<()> {
        assert_eq!(n, self.root.next_blob, "map nodes are numbered in order");
        self.root.next_blob += 1;
        self.fresh.push((n, node.frame()?, None));
        self.nodes.push((n, node));
        Ok(())
    }
}

/// A file a flush writes.
struct FileWrite<'a> {
    id: u32,
    /// The base it was opened with.
    base: Option<&'a Arc<dyn Base>>,
    /// It was made with a base, and opened without it.
    missing: bool,
    written: &'a Written,
    /// Its size after the flush.
    size: u64,
}

impl Snapshot {
    /// Open the namespace: see the module documentation. An owner without a
    /// cache, or whose last flush had an unknown outcome, syncs alongside, so
    /// that what it reads is durable; a `reader` reads what is stored.
    pub async fn open(
        store: Arc<dyn KvStore>,
        options: StateOptions,
        cache: Option<NamespaceCache>,
        reader: bool,
    ) -> io::Result<Self> {
        let owner = cache.as_ref().filter(|_| !reader);
        let unsettled = owner.and_then(NamespaceCache::unsettled);
        let settle = !reader && (owner.is_none() || unsettled.is_some());
        let settle = async {
            match settle {
                true => store.sync().await,
                false => Ok(()),
            }
        };
        let (root_bytes, ()) = futures::try_join!(store.get(ROOT), settle)?;
        if let (Some(owner), Some(unknown)) = (owner, unsettled) {
            owner.settled(unknown);
        }
        let root = match &root_bytes {
            Some(bytes) => Root::decode(bytes)?,
            None if !(1..=MAX_CHUNK_PAGES).contains(&options.chunk_pages) => {
                return refuse(format!("chunk_pages {} is invalid", options.chunk_pages));
            }
            None if !(2..=MAX_FANOUT).contains(&options.fanout) => {
                return refuse(format!("fanout {} is invalid", options.fanout));
            }
            None => Root::new(options.chunk_pages, options.fanout),
        };
        let mut snapshot = Self {
            store,
            options: options.clone(),
            cache: cache.clone(),
            root: Arc::new(root),
            root_bytes,
            records: Records::new(),
            bases: BTreeMap::new(),
            unchanged: false,
            reader,
            images: Arc::default(),
            nodes: Arc::default(),
            fetching: Arc::default(),
            readahead: Mutex::default(),
        };
        // The large records' blobs and every chunk of a small file, together,
        // beside the maps of all but the largest files.
        let (small, fetched) = {
            let snapshot = &snapshot;
            let small = async {
                let mut wanted = BTreeSet::new();
                let mut small = Vec::new();
                for (&id, entry) in &snapshot.root.directory {
                    match &entry.kind {
                        Kind::Record(Value::Blob(n)) => {
                            wanted.insert(*n);
                        }
                        Kind::Paged {
                            size, base: None, ..
                        } if size.div_ceil(PAGE) <= options.prefetch_pages => {
                            let numbers: Vec<u64> =
                                (0..snapshot.root.shape(*size).count).collect();
                            let chunks = snapshot.chunks(id, &numbers).await?;
                            for (chunk, stored) in numbers.into_iter().zip(chunks) {
                                if stored.current().is_some() {
                                    wanted.extend(stored.blobs());
                                    small.push((id, chunk, stored));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let wanted: Vec<u64> = wanted.into_iter().collect();
                let blobs = snapshot.blobs(&wanted).await?;
                let fetched: HashMap<u64, Bytes> = wanted.into_iter().zip(blobs).collect();
                Ok::<_, io::Error>((small, fetched))
            };
            futures::try_join!(small, snapshot.prefetch_maps())?.0
        };
        for (id, chunk, stored) in small {
            snapshot.decode(id, chunk, stored, &fetched)?;
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
        if let Some(owner) = owner {
            let root = &snapshot.root;
            let known = owner.validate(snapshot.root_bytes.as_deref());
            snapshot.unchanged = known && owner.committed() == Some(root.version);
            owner.record(
                snapshot.root_bytes.clone(),
                (!known).then_some(root.version),
            );
        }
        Ok(snapshot)
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

    /// The map of the paged file `id`, and its shape.
    fn map(&self, id: u32) -> Option<(&Map, Shape)> {
        match &self.root.directory.get(&id)?.kind {
            Kind::Paged { size, map, .. } => Some((map, self.root.shape(*size))),
            Kind::Record(_) => None,
        }
    }

    /// Run `walk` over the map nodes it needs: those decoded, and those it
    /// lacked, fetched together (a level of the map a round trip), until it
    /// lacks none.
    async fn with_nodes<T>(&self, mut walk: impl FnMut(&Nodes) -> T) -> io::Result<T> {
        let mut fetched: HashMap<u64, Arc<Node>> = HashMap::new();
        loop {
            let (out, missing) = {
                let get = |n: u64| {
                    let kept = || self.nodes.lock().unwrap().get(&n).cloned();
                    fetched.get(&n).cloned().or_else(kept)
                };
                let nodes = Nodes::new(&get);
                (walk(&nodes), nodes.missing())
            };
            if missing.is_empty() {
                return Ok(out);
            }
            let blobs: Vec<u64> = missing.iter().map(|need| need.blob).collect();
            for (need, frame) in missing.into_iter().zip(self.blobs(&blobs).await?) {
                let node = Arc::new(Node::from_frame(&frame, need)?);
                fetched.insert(need.blob, node.clone());
                self.keep_nodes([(need.blob, node)]);
            }
        }
    }

    /// The map nodes of every file of at most [`PREFETCH_MAP_NODES`], a
    /// level of all of them a round trip.
    async fn prefetch_maps(&self) -> io::Result<()> {
        let maps: Vec<(&Map, Shape)> = (self.root.directory.keys())
            .filter_map(|&id| self.map(id))
            .filter(|(_, shape)| (1..=PREFETCH_MAP_NODES).contains(&shape.nodes()))
            .collect();
        self.with_nodes(|nodes| {
            for &(map, shape) in &maps {
                map.walk(shape, nodes, &mut |_| {});
            }
        })
        .await
    }

    /// Chunks `chunks` of the file `id`, each below its count, as stored.
    async fn chunks(&self, id: u32, chunks: &[u64]) -> io::Result<Vec<Chunk>> {
        let Some((map, shape)) = self.map(id) else {
            return Ok(vec![Chunk::default(); chunks.len()]);
        };
        let get = |nodes: &Nodes| (chunks.iter()).map(|&c| map.get(shape, c, nodes)).collect();
        self.with_nodes(get).await
    }

    /// Page `page` of the file `name`; `None` for zeros (a hole of a file
    /// without a base, or past its end).
    pub async fn read(&self, name: &str, page: u64) -> io::Result<Option<Page>> {
        let Some(id) = self.root.file_id(name) else {
            return Ok(None);
        };
        let (chunk, at) = (page / self.root.chunk_pages, page % self.root.chunk_pages);
        let at = at as usize * PAGE as usize;
        if self.map(id).is_none_or(|(_, shape)| chunk >= shape.count) {
            return Ok(None);
        }
        let stored = self.chunks(id, &[chunk]).await?[0];
        if stored.current().is_none() {
            return Ok(self
                .base_of(id)?
                .and_then(|base| base.page(page))
                .map(Page::copy_from_slice));
        }
        let image = self.image(id, chunk, stored).await?;
        let image = image.pages.slice(at..at + PAGE as usize);
        // Zeros where the base has no page read as a hole.
        let based = self
            .base_of(id)?
            .is_some_and(|base| base.page(page).is_some());
        Ok((based || image.iter().any(|&byte| byte != 0)).then_some(image))
    }

    /// Chunk `chunk` of the file `id`, `stored` in a blob: decoded, or
    /// fetched with its base, and with the chunks after it when reads are
    /// sequential, in one round trip.
    async fn image(&self, id: u32, chunk: u64, stored: Chunk) -> io::Result<Arc<Image>> {
        let current = stored.current().expect("a stored chunk");
        if let Some(image) = self.images.lock().unwrap().get(&current) {
            return Ok(image.clone());
        }
        let ahead = self.readahead.lock().unwrap().miss(id, chunk);
        let count = self.map(id).map_or(0, |(_, shape)| shape.count);
        let after: Vec<u64> = (chunk + 1..(chunk + 1 + ahead).min(count)).collect();
        let after = (after.iter().copied())
            .zip(self.chunks(id, &after).await?)
            .filter(|&(_, stored)| !self.has(stored));
        let wanted = std::iter::once((chunk, stored)).chain(after).collect();
        let mut images = self.fetch(id, wanted).await?;
        Ok(images.swap_remove(0))
    }

    /// Chunks `wanted` of the file `id`, as stored in blobs: the blobs
    /// fetched together, in one round trip, and decoded (and kept).
    async fn fetch(&self, id: u32, wanted: Vec<(u64, Chunk)>) -> io::Result<Vec<Arc<Image>>> {
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let blobs: Vec<u64> = (wanted.iter())
            .flat_map(|(_, stored)| stored.blobs())
            .collect();
        let fetched: HashMap<u64, Bytes> = blobs
            .iter()
            .copied()
            .zip(self.blobs(&blobs).await?)
            .collect();
        (wanted.into_iter())
            .map(|(chunk, stored)| self.decode(id, chunk, stored, &fetched))
            .collect()
    }

    /// The base blob `n` of a chunk, decoded.
    async fn base_image(&self, id: u32, chunk: u64, n: u64) -> io::Result<Arc<Image>> {
        if let Some(image) = self.images.lock().unwrap().get(&n) {
            return Ok(image.clone());
        }
        let frame = self.blob(n).await?;
        let prefix = reference(self.base_of(id)?, self.root.chunk_pages, chunk);
        decode_chunk(&frame, &prefix, self.root.chunk_bytes())
    }

    fn has(&self, chunk: Chunk) -> bool {
        chunk
            .current()
            .is_none_or(|n| self.images.lock().unwrap().contains_key(&n))
    }

    /// Decode chunk `chunk` of the file `id`, `stored` in blobs that
    /// `fetched` holds, and keep its base and its pages now, which it
    /// returns.
    fn decode(
        &self,
        id: u32,
        chunk: u64,
        stored: Chunk,
        fetched: &HashMap<u64, Bytes>,
    ) -> io::Result<Arc<Image>> {
        let size = self.root.chunk_bytes();
        let decode = |n: u64, prefix: &[u8]| decode_chunk(&fetched[&n], prefix, size);
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
        let current = delta
            .as_ref()
            .or(base.as_ref())
            .expect("a stored chunk")
            .1
            .clone();
        self.keep(base.into_iter().chain(delta));
        Ok(current)
    }

    /// Keep decoded images, dropping them all first once there are
    /// [`KEPT_IMAGES`].
    fn keep(&self, decoded: impl IntoIterator<Item = (u64, Arc<Image>)>) {
        let mut images = self.images.lock().unwrap();
        for (n, image) in decoded {
            if images.len() >= KEPT_IMAGES {
                images.clear();
            }
            images.insert(n, image);
        }
    }

    /// Keep decoded map nodes, dropping them all first once there are
    /// [`KEPT_NODES`].
    fn keep_nodes(&self, decoded: impl IntoIterator<Item = (u64, Arc<Node>)>) {
        let mut nodes = self.nodes.lock().unwrap();
        for (n, node) in decoded {
            if nodes.len() >= KEPT_NODES {
                nodes.clear();
            }
            nodes.insert(n, node);
        }
    }

    /// Blobs `wanted`, in order: from the cache, or the store, together.
    async fn blobs(&self, wanted: &[u64]) -> io::Result<Vec<Bytes>> {
        try_join_all(wanted.iter().map(|&n| self.blob(n))).await
    }

    async fn blob(&self, n: u64) -> io::Result<Bytes> {
        let cache = self.cache.clone();
        if let Some(cache) = &cache
            && let Some(value) = cache.get(self.root.id, n).await
        {
            return Ok(value);
        }
        // A reader's root may be a write not yet durable (as the store shows
        // it), whose blobs a crash could lose and the next flush reuse: only
        // the owner, whose roots are durable, caches what it fetches.
        let cache = cache.filter(|_| !self.reader);
        let database = self.root.id;
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
                        cache.insert(database, n, value.clone());
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
        // Gone once no read awaits it, unless a newer fetch took its place.
        let mut fetching = self.fetching.lock().unwrap();
        if fetching
            .get(&n)
            .is_some_and(|fetch| fetch.upgrade().is_none())
        {
            fetching.remove(&n);
        }
        drop(fetching);
        value.map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }

    /// Refuse what no flush could store, before anything is written.
    pub(crate) async fn check(&self, writes: &Writes, records: &Records) -> io::Result<()> {
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
            if self.lacks_its_base(name) && !self.drops_chunks_only(name, written).await? {
                return refuse(format!("{name:?} has a base: open it with it"));
            }
        }
        let new = (records.keys().chain(writes.keys()))
            .filter(|name| self.root.file_id(name).is_none())
            .count();
        if u64::from(self.root.next_file) + new as u64 > u64::from(MAX_FILES) {
            return refuse("too many files".into());
        }
        Ok(())
    }

    /// The file `name` was made with a base, and opened without it.
    fn lacks_its_base(&self, name: &str) -> bool {
        let id = self.root.file_id(name);
        let based = id.is_some_and(|id| {
            matches!(
                self.root.directory[&id].kind,
                Kind::Paged { base: Some(_), .. }
            )
        });
        based && !self.bases.contains_key(name)
    }

    /// `written` only makes whole chunks holes, or cuts the file's length:
    /// all a file can do without its base, whose chunks it cannot decode.
    async fn drops_chunks_only(&self, name: &str, written: &Written) -> io::Result<bool> {
        let Some(id) = self.root.file_id(name) else {
            return Ok(true);
        };
        if !written.pages.values().all(Option::is_none) {
            return Ok(false);
        }
        let current = self.file_size(name);
        let size = written.size_after(current);
        let per = self.root.chunk_pages;
        let count = self.root.shape(current).count;
        let touched: Vec<u64> = touched(written, current, per).collect();
        let held: Vec<u64> = touched.iter().copied().filter(|&c| c < count).collect();
        let stored: HashMap<u64, Chunk> = (held.iter().copied())
            .zip(self.chunks(id, &held).await?)
            .collect();
        Ok(touched.into_iter().all(|chunk| {
            let current = stored.get(&chunk).and_then(|c| c.current());
            (chunk * per..(chunk + 1) * per)
                .all(|page| page * PAGE >= size || written.pages.contains_key(&page))
                || current.is_none()
        }))
    }

    /// Flush `writes` and `records`, which [`Self::check`] passed: see the
    /// module documentation.
    pub(crate) async fn commit(
        &self,
        mut writes: Writes,
        mut records: Records,
    ) -> io::Result<(FlushReport, Self)> {
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
        let mut flush = Flush::new(&self.root);
        for (name, written) in &writes {
            self.commit_file(&mut flush, name, written).await?;
        }
        let records = self.commit_records(&mut flush, records)?;
        self.write(flush, records).await
    }

    /// Write `written` into the file `name` of `flush`'s root.
    async fn commit_file(
        &self,
        flush: &mut Flush,
        name: &str,
        written: &Written,
    ) -> io::Result<()> {
        let base = self.bases.get(name);
        let id = flush.file(name, || Kind::Paged {
            size: 0,
            base: base.map(|base| base.id()),
            map: Map::empty(),
        });
        let current = self.file_size(name);
        let size = written.size_after(current);
        let (old, new) = (flush.root.shape(current), flush.root.shape(size));
        let based = matches!(
            flush.root.directory[&id].kind,
            Kind::Paged { base: Some(_), .. }
        );
        let file = FileWrite {
            id,
            base,
            // Without its base, a file made with one only drops chunks
            // (checked).
            missing: based && base.is_none(),
            written,
            size,
        };
        let touched: Vec<u64> = touched(written, current, flush.root.chunk_pages).collect();
        // The stored ones, and their pages, together.
        let held: Vec<u64> = touched.iter().copied().filter(|&c| c < old.count).collect();
        let stored: HashMap<u64, Chunk> = (held.iter().copied())
            .zip(self.chunks(id, &held).await?)
            .collect();
        if !file.missing {
            let undecoded = (stored.iter())
                .filter(|&(_, &chunk)| !self.has(chunk))
                .map(|(&c, &chunk)| (c, chunk));
            self.fetch(id, undecoded.collect()).await?;
        }
        let mut changes = BTreeMap::new();
        for chunk in touched {
            let held = stored.get(&chunk).copied().unwrap_or_default();
            let now = self.commit_chunk(flush, &file, chunk, held).await?;
            if now != held {
                changes.insert(chunk, now);
            }
        }
        // The map, with the nodes on the changed paths put anew.
        let Some(Entry {
            kind: Kind::Paged { map, .. },
            ..
        }) = flush.root.directory.get(&id)
        else {
            unreachable!("checked: a paged file");
        };
        let (map, next) = (map.clone(), flush.root.next_blob);
        let update = (self.with_nodes(|nodes| map.update(old, new, &changes, nodes, next))).await?;
        for (n, node) in update.fresh {
            flush.put_node(n, node)?;
        }
        flush.dead.extend(update.dead);
        let Some(Entry {
            kind:
                Kind::Paged {
                    size: held_size,
                    map: held_map,
                    ..
                },
            ..
        }) = flush.root.directory.get_mut(&id)
        else {
            unreachable!("checked: a paged file");
        };
        *held_size = size;
        *held_map = update.map;
        Ok(())
    }

    /// Chunk `chunk` of `file`, `stored` until now, with its writes: as it
    /// was, holes only (nothing stored), a delta against its base while that
    /// stays small beside the base, or else a new base.
    async fn commit_chunk(
        &self,
        flush: &mut Flush,
        file: &FileWrite<'_>,
        chunk: u64,
        stored: Chunk,
    ) -> io::Result<Chunk> {
        let per = flush.root.chunk_pages;
        let hole = implicit(file.base, per, chunk);
        let (base_image, now) = match stored.current() {
            Some(_) if !file.missing => {
                let base_image = match stored.base {
                    Some(n) => Some(self.base_image(file.id, chunk, n).await?),
                    None => None,
                };
                (
                    base_image,
                    self.image(file.id, chunk, stored).await?.pages.to_vec(),
                )
            }
            _ => (None, hole.clone()),
        };
        let mut pages = now.clone();
        for (i, page) in pages.chunks_mut(PAGE as usize).enumerate() {
            let index = chunk * per + i as u64;
            let hole = &hole[i * PAGE as usize..(i + 1) * PAGE as usize];
            match file.written.pages.get(&index) {
                _ if index * PAGE >= file.size => page.copy_from_slice(hole),
                Some(image) => page.copy_from_slice(image.as_deref().unwrap_or(hole)),
                None => {}
            }
        }
        if pages == now && stored.current().is_some() && !file.missing {
            return Ok(stored);
        }
        flush.dead.extend(stored.delta);
        if pages == hole || file.missing {
            flush.dead.extend(stored.base);
            return Ok(Chunk::default());
        }
        let pages = Bytes::from(pages);
        if let Some(base) = &base_image {
            let delta = zstd::compress(&pages, &base.pages, Bytes::copy_from_slice)?;
            if delta.len() as f64 <= self.options.rebase_ratio * base.stored as f64 {
                return Ok(Chunk {
                    base: stored.base,
                    delta: Some(flush.put(delta, Some(pages))),
                });
            }
        }
        // Against its file's base pages, if it has them.
        let prefix = if file.base.is_some() { &hole[..] } else { &[] };
        let frame = zstd::compress(&pages, prefix, Bytes::copy_from_slice)?;
        flush.rebased += usize::from(stored.base.is_some());
        flush.dead.extend(stored.base);
        Ok(Chunk {
            base: Some(flush.put(frame, Some(pages))),
            delta: None,
        })
    }

    /// Set `records` in `flush`'s root, inline or in blobs of their own:
    /// the records after it.
    fn commit_records(&self, flush: &mut Flush, records: Records) -> io::Result<Records> {
        let mut after = self.records.clone();
        for (name, value) in records {
            let id = flush.file(&name, || Kind::Record(Value::Inline(Bytes::new())));
            if let Kind::Record(Value::Blob(n)) = flush.root.directory[&id].kind {
                flush.dead.push(n);
            }
            let stored = match value.len() <= self.options.inline_record_bytes {
                true => Value::Inline(value.clone()),
                false => {
                    let frame = zstd::compress(&value, &[], Bytes::copy_from_slice)?;
                    Value::Blob(flush.put(frame, None))
                }
            };
            flush.root.directory.get_mut(&id).expect("allocated").kind = Kind::Record(stored);
            after.insert(name, value);
        }
        Ok(after)
    }

    /// Write `flush`: delete the garbage past its grace, put the blobs, then
    /// the root that names them; then one sync.
    async fn write(&self, flush: Flush, records: Records) -> io::Result<(FlushReport, Self)> {
        let Flush {
            mut root,
            fresh,
            dead,
            nodes,
            rebased,
        } = flush;
        let cache = self.cache.as_ref();
        if cache.is_some_and(|cache| !cache.claim(self.version())) {
            return refuse("another state flushed since this one opened; reopen".into());
        }
        let version = root.version;
        // The garbage old enough that no reader holds a root naming it.
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
                cache.insert(root.id, *n, value.clone());
            }
            cache.flushed(bytes.clone(), version);
        }
        let mut after = self.continued(Arc::new(root), Some(bytes));
        after.records = records;
        after.keep(
            fresh
                .into_iter()
                .filter_map(|(n, _, image)| Some((n, image?))),
        );
        after.keep_nodes(nodes);
        let report = FlushReport {
            blob_bytes,
            root_bytes,
            rebased,
        };
        Ok((report, after))
    }

    fn failed(&self, error: io::Error) -> io::Error {
        if let Some(cache) = &self.cache {
            cache.forget();
        }
        error
    }

    fn continued(&self, root: Arc<Root>, root_bytes: Option<Bytes>) -> Self {
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            cache: self.cache.clone(),
            root,
            root_bytes,
            records: self.records.clone(),
            bases: self.bases.clone(),
            unchanged: false,
            reader: self.reader,
            images: self.images.clone(),
            nodes: self.nodes.clone(),
            fetching: self.fetching.clone(),
            readahead: Mutex::default(),
        }
    }

    pub(crate) fn with_base(&self, name: &str, base: Arc<dyn Base>) -> io::Result<Self> {
        if base.id() == u64::MAX {
            return refuse("a base's id must be below u64::MAX".into());
        }
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

#[cfg(test)]
#[path = "../tests/unit/store.rs"]
mod tests;

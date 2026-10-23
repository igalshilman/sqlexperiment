//! Experiment (the `cow` feature): copy-on-write pages, in place of the log
//! and its checkpoints ([`crate::snapshot`]), behind the same interface.
//!
//! A flush writes the pages it changed, and the records too large for the
//! root, as one immutable value, a blob: `cow/blob/<n>`, at a number no root
//! has used, its slots compressed together in one zstd frame. Then it puts
//! the root (`cow/root`), which maps each page of each file to the blob slot
//! holding its image, and syncs once. The root is the commit point: the store
//! keeps writes in issue order, so a root is stored only with the blobs it
//! names. Nothing is overwritten: there is no log to replay, no checkpoint,
//! and a page is written once per change, whole (compressed with the other
//! pages of its flush, not against its previous image).
//!
//! A blob lives while any of its slots is mapped. A flush copies the live
//! slots of blobs mostly dead (below [`compact_below`] of their slots) into
//! its own blob, so their keys can go. A blob no root maps is garbage: the
//! root lists it, and the next flush deletes it, unless another state of the
//! owner is open (it may still read it).
//!
//! The root is one zstd frame: the directory, each paged file's map as runs
//! of pages in consecutive slots of one blob, and the records of at most
//! [`crate::StateOptions::inline_log_bytes`].
use crate::{
    PAGE,
    cache::{NamespaceCache, Reader},
    kv::KvStore,
    log::{self, Base, Records, file_of, get_varint, page_id, page_of, put_varint},
    page_codec::Page,
    snapshot::{FlushReport, Settings, Writes},
    zstd,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex, OnceLock},
};

const ROOT: &str = "cow/root";
const ROOT_MAGIC: &[u8; 4] = b"SR01";
const BLOB_MAGIC: &[u8; 4] = b"SB01";
const BLOB_PREFIX: &str = "cow/blob/";
/// Largest decoded root or blob.
const MAX_DECODED: usize = 256 << 20;
/// Most slots compaction adds to one flush.
const MAX_COMPACTED: usize = 256;
/// Decoded blobs a state keeps in memory before it drops them all.
const KEPT_BLOBS: usize = 1024;

fn blob_key(n: u64) -> String {
    format!("{BLOB_PREFIX}{n:016x}")
}

fn knob(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Most slots of a blob (`COW_BLOB_PAGES`, 64): a cold read fetches its
/// whole blob.
pub(crate) fn blob_slots() -> usize {
    static SLOTS: OnceLock<usize> = OnceLock::new();
    *SLOTS.get_or_init(|| knob("COW_BLOB_PAGES", 64.0).max(1.0) as usize)
}

/// A blob with fewer live slots than this fraction of its slots
/// (`COW_COMPACT`, 0.5) is copied forward by the next flush.
pub(crate) fn compact_below() -> f64 {
    static BELOW: OnceLock<f64> = OnceLock::new();
    *BELOW.get_or_init(|| knob("COW_COMPACT", 0.5))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
}

/// A slot of a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Slot {
    blob: u64,
    index: u32,
}

impl Slot {
    /// Its id in the page cache: blobs are immutable, so it never changes.
    fn cache_id(self) -> u64 {
        self.blob << 20 | u64::from(self.index)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Inline(Bytes),
    In(Slot),
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Record(Value),
    /// Its size, its base's id, and the slot of each page it has; any other
    /// page is a hole.
    Paged {
        size: u64,
        base: Option<u64>,
        map: BTreeMap<u64, Slot>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    kind: Kind,
}

/// The root: the commit point.
#[derive(Clone, Debug, Default, PartialEq)]
struct Root {
    version: u64,
    next_blob: u64,
    /// Blobs no root names any more, to delete.
    garbage: BTreeSet<u64>,
    /// The blobs it names, with their slot counts.
    blobs: BTreeMap<u64, u32>,
    next_file: u32,
    directory: BTreeMap<u32, Entry>,
}

struct Reading<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reading<'_> {
    fn varint(&mut self) -> io::Result<u64> {
        get_varint(self.bytes, &mut self.at).ok_or_else(|| invalid("root"))
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

    fn slot(&mut self) -> io::Result<Slot> {
        let blob = self.varint()?;
        let index = u32::try_from(self.varint()?).map_err(|_| invalid("root"))?;
        Ok(Slot { blob, index })
    }
}

impl Root {
    fn file_id(&self, name: &str) -> Option<u32> {
        (self.directory.iter())
            .find(|(_, entry)| entry.name == name)
            .map(|(&id, _)| id)
    }

    fn size(&self, file: u32) -> u64 {
        match self.directory.get(&file).map(|entry| &entry.kind) {
            Some(Kind::Paged { size, .. }) => *size,
            _ => 0,
        }
    }

    /// Every mapped slot, with what maps it.
    fn slots(&self) -> impl Iterator<Item = (Target, Slot)> + '_ {
        self.directory.iter().flat_map(|(&id, entry)| {
            let slots: Box<dyn Iterator<Item = (Target, Slot)>> = match &entry.kind {
                Kind::Record(Value::In(slot)) => {
                    Box::new(std::iter::once((Target::Record(id), *slot)))
                }
                Kind::Record(Value::Inline(_)) => Box::new(std::iter::empty()),
                Kind::Paged { map, .. } => Box::new(
                    map.iter()
                        .map(move |(&page, &slot)| (Target::Page(page_id(id, page)), slot)),
                ),
            };
            slots
        })
    }

    /// Live slots per blob.
    fn live(&self) -> BTreeMap<u64, u32> {
        let mut live = BTreeMap::new();
        for (_, slot) in self.slots() {
            *live.entry(slot.blob).or_default() += 1;
        }
        live
    }

    /// The blobs holding records.
    fn record_blobs(&self) -> BTreeSet<u64> {
        (self.directory.values())
            .filter_map(|entry| match &entry.kind {
                Kind::Record(Value::In(slot)) => Some(slot.blob),
                _ => None,
            })
            .collect()
    }

    fn encode(&self) -> Bytes {
        let mut body = Vec::new();
        put_varint(&mut body, self.version);
        put_varint(&mut body, self.next_blob);
        put_varint(&mut body, self.garbage.len() as u64);
        let mut previous = 0;
        for &n in &self.garbage {
            put_varint(&mut body, n - previous);
            previous = n;
        }
        put_varint(&mut body, self.blobs.len() as u64);
        let mut previous = 0;
        for (&n, &slots) in &self.blobs {
            put_varint(&mut body, n - previous);
            put_varint(&mut body, u64::from(slots));
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
                Kind::Record(Value::In(slot)) => {
                    body.push(1);
                    put_varint(&mut body, slot.blob);
                    put_varint(&mut body, u64::from(slot.index));
                }
                Kind::Paged { size, base, map } => {
                    body.push(2);
                    put_varint(&mut body, size / 512);
                    match base {
                        Some(base) => {
                            body.push(1);
                            put_varint(&mut body, *base);
                        }
                        None => body.push(0),
                    }
                    // Runs of pages in consecutive slots of one blob.
                    let mut runs: Vec<(u64, u64, Slot)> = Vec::new();
                    for (&page, &slot) in map {
                        match runs.last_mut() {
                            Some((start, len, first))
                                if *start + *len == page
                                    && first.blob == slot.blob
                                    && u64::from(first.index) + *len == u64::from(slot.index) =>
                            {
                                *len += 1;
                            }
                            _ => runs.push((page, 1, slot)),
                        }
                    }
                    put_varint(&mut body, runs.len() as u64);
                    let mut end = 0;
                    for (start, len, slot) in runs {
                        put_varint(&mut body, start - end);
                        put_varint(&mut body, len);
                        put_varint(&mut body, slot.blob);
                        put_varint(&mut body, u64::from(slot.index));
                        end = start + len;
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

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let rest = bytes
            .strip_prefix(ROOT_MAGIC)
            .ok_or_else(|| invalid("root"))?;
        let mut at = 0;
        let len = get_varint(rest, &mut at).ok_or_else(|| invalid("root"))? as usize;
        if len > MAX_DECODED {
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
        let mut root = Root {
            version: r.varint()?,
            next_blob: r.varint()?,
            ..Root::default()
        };
        let mut previous = 0;
        for _ in 0..r.varint()? {
            previous += r.varint()?;
            root.garbage.insert(previous);
        }
        let mut previous = 0;
        for _ in 0..r.varint()? {
            previous += r.varint()?;
            let slots = u32::try_from(r.varint()?).map_err(|_| invalid("root"))?;
            root.blobs.insert(previous, slots);
        }
        root.next_file = u32::try_from(r.varint()?).map_err(|_| invalid("root"))?;
        let mut next = 0u32;
        for _ in 0..r.varint()? {
            let id = next
                .checked_add(u32::try_from(r.varint()?).map_err(|_| invalid("root"))?)
                .ok_or_else(|| invalid("root"))?;
            next = id + 1;
            let name = String::from_utf8(r.bytes()?.to_vec()).map_err(|_| invalid("root"))?;
            let tag = r.varint()?;
            let kind = match tag {
                0 => Kind::Record(Value::Inline(Bytes::copy_from_slice(r.bytes()?))),
                1 => Kind::Record(Value::In(r.slot()?)),
                2 => {
                    let size = r.varint()? * 512;
                    let base = match r.varint()? {
                        0 => None,
                        _ => Some(r.varint()?),
                    };
                    let mut map = BTreeMap::new();
                    let mut end = 0;
                    for _ in 0..r.varint()? {
                        let start = end + r.varint()?;
                        let len = r.varint()?;
                        let first = r.slot()?;
                        for i in 0..len {
                            let index = u32::try_from(u64::from(first.index) + i)
                                .map_err(|_| invalid("root"))?;
                            map.insert(
                                start + i,
                                Slot {
                                    blob: first.blob,
                                    index,
                                },
                            );
                        }
                        end = start + len;
                    }
                    Kind::Paged { size, base, map }
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

/// What a slot holds: a page (by page id) or a record (by file id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Page(u64),
    Record(u32),
}

/// A blob: `SB01`, the slot count, each slot's length, then one zstd frame
/// (with a checksum) of the slots in order.
fn encode_blob(slots: &[Bytes]) -> io::Result<Bytes> {
    let mut out = BLOB_MAGIC.to_vec();
    put_varint(&mut out, slots.len() as u64);
    for slot in slots {
        put_varint(&mut out, slot.len() as u64);
    }
    let input: Vec<u8> = slots.concat();
    zstd::compress(&input, &[], true, |frame| out.extend_from_slice(frame))?;
    Ok(Bytes::from(out))
}

fn decode_blob(bytes: &[u8]) -> io::Result<Vec<Bytes>> {
    let rest = bytes
        .strip_prefix(BLOB_MAGIC)
        .ok_or_else(|| invalid("blob"))?;
    let mut at = 0;
    let count = get_varint(rest, &mut at).ok_or_else(|| invalid("blob"))? as usize;
    let mut lens = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        lens.push(get_varint(rest, &mut at).ok_or_else(|| invalid("blob"))? as usize);
    }
    let total: usize = lens.iter().sum();
    if total > MAX_DECODED {
        return Err(invalid("blob"));
    }
    let mut out = Vec::with_capacity(total);
    zstd::decompress(&mut out, &rest[at..], &[])?;
    if out.len() != total {
        return Err(invalid("blob"));
    }
    let out = Bytes::from(out);
    let mut start = 0;
    Ok(lens
        .into_iter()
        .map(|len| {
            let slot = out.slice(start..start + len);
            start += len;
            slot
        })
        .collect())
}

type Decoded = Arc<Vec<Bytes>>;

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: Settings,
    root: Root,
    root_bytes: Option<Bytes>,
    records: Records,
    bases: BTreeMap<String, Arc<dyn Base>>,
    /// Nothing changed since this owner last saw the namespace.
    pub(crate) unchanged: bool,
    /// Blobs decoded, by number: immutable, so every view shares them.
    blobs: Arc<Mutex<HashMap<u64, Decoded>>>,
    /// The last page read.
    page: Mutex<Option<(u64, Page)>>,
    reader: Mutex<Option<Reader>>,
}

impl Snapshot {
    pub async fn open(
        store: Arc<dyn KvStore>,
        options: Settings,
        reader: bool,
    ) -> io::Result<Self> {
        let cache = options.cache.as_ref();
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
            None => Root::default(),
        };
        let known = cache.is_some_and(|c| c.validate(root_bytes.as_deref(), 0));
        let cold = cache.is_none_or(|cache| !cache.knows_epoch(0));
        // The blobs of records, and, cold, every blob of a small file.
        let record_blobs = root.record_blobs();
        let mut wanted = record_blobs.clone();
        if cold {
            for entry in root.directory.values() {
                if let Kind::Paged { size, map, .. } = &entry.kind
                    && size.div_ceil(PAGE) <= options.prefetch_pages
                {
                    wanted.extend(map.values().map(|slot| slot.blob));
                }
            }
        }
        let held = cache.map_or_else(log::Values::new, |cache| cache.values(&record_blobs));
        let missing: Vec<u64> = wanted
            .iter()
            .copied()
            .filter(|n| !held.contains_key(n))
            .collect();
        let keys: Vec<String> = missing.iter().map(|&n| blob_key(n)).collect();
        let fetched = try_join_all(keys.iter().map(|key| store.get(key))).await?;
        let mut values = held;
        for (n, value) in missing.into_iter().zip(fetched) {
            values.insert(n, value.ok_or_else(|| invalid("root: a blob is missing"))?);
        }
        let mut blobs = HashMap::new();
        for (&n, value) in &values {
            blobs.insert(n, Arc::new(decode_blob(value)?));
        }
        let mut records = Records::new();
        for entry in root.directory.values() {
            if let Kind::Record(value) = &entry.kind {
                let value = match value {
                    Value::Inline(bytes) => bytes.clone(),
                    Value::In(slot) => slot_of(&blobs, *slot)?,
                };
                records.insert(entry.name.clone(), value);
            }
        }
        if let Some(cache) = cache {
            let kept = values
                .into_iter()
                .filter(|(n, _)| record_blobs.contains(n))
                .collect();
            cache.keep_values(&record_blobs, kept);
        }
        let unchanged = known && cache.and_then(NamespaceCache::committed) == Some(root.version);
        if let Some(cache) = cache {
            let committed = (!known).then_some(root.version);
            cache.record(root_bytes.clone(), 0, committed);
        }
        let reader = cache.map(|cache| cache.reader(root.version));
        Ok(Self {
            store,
            options,
            root,
            root_bytes,
            records,
            bases: BTreeMap::new(),
            unchanged,
            blobs: Arc::new(Mutex::new(blobs)),
            page: Mutex::default(),
            reader: Mutex::new(reader),
        })
    }

    pub(crate) async fn is_current(&self) -> io::Result<bool> {
        Ok(self.store.get(ROOT).await? == self.root_bytes)
    }

    pub fn version(&self) -> u64 {
        self.root.version
    }

    pub fn file_size(&self, name: &str) -> u64 {
        self.root.file_id(name).map_or(0, |id| self.root.size(id))
    }

    pub async fn read(&self, name: &str, page: u64) -> io::Result<Option<Page>> {
        match self.root.file_id(name) {
            Some(id) => self.block(page_id(id, page)).await,
            None => Ok(None),
        }
    }

    pub fn records(&self) -> &Records {
        &self.records
    }

    pub(crate) fn is_paged(&self, name: &str) -> Option<bool> {
        let id = self.root.file_id(name)?;
        Some(matches!(self.root.directory[&id].kind, Kind::Paged { .. }))
    }

    fn base_of(&self, file: u32) -> io::Result<Option<&Arc<dyn Base>>> {
        let Some(entry) = self.root.directory.get(&file) else {
            return Ok(None);
        };
        match (&entry.kind, self.bases.get(&entry.name)) {
            (Kind::Paged { base: Some(_), .. }, None) => Err(io::Error::other(format!(
                "{:?} has a base: open it with it",
                entry.name
            ))),
            (_, base) => Ok(base),
        }
    }

    fn hole(&self, index: u64) -> io::Result<Option<Page>> {
        let base = self.base_of(file_of(index))?;
        Ok(base
            .and_then(|base| base.page(page_of(index)))
            .map(Page::copy_from_slice))
    }

    /// Page `index`: its slot's image, from this state's decoded blobs, the
    /// page cache, or its blob, fetched whole; a page without a slot is a
    /// hole.
    pub async fn block(&self, index: u64) -> io::Result<Option<Page>> {
        if let Some((cached, page)) = &*self.page.lock().unwrap()
            && *cached == index
        {
            return Ok(Some(page.clone()));
        }
        let slot = match self.root.directory.get(&file_of(index)).map(|e| &e.kind) {
            Some(Kind::Paged { map, .. }) => map.get(&page_of(index)).copied(),
            _ => None,
        };
        let Some(slot) = slot else {
            return self.hole(index);
        };
        let held = self.blobs.lock().unwrap().get(&slot.blob).cloned();
        let page = match held {
            Some(blob) => slot_in(&blob, slot)?,
            None => {
                let cache = self.options.cache.as_ref();
                match match cache {
                    Some(cache) => cache.get(slot.cache_id()).await,
                    None => None,
                } {
                    Some(page) => page,
                    None => {
                        let blob = self.fetch(slot.blob).await?;
                        slot_in(&blob, slot)?
                    }
                }
            }
        };
        *self.page.lock().unwrap() = Some((index, page.clone()));
        Ok(Some(page))
    }

    /// Get blob `n` and decode it; its pages go to the page cache.
    async fn fetch(&self, n: u64) -> io::Result<Decoded> {
        let cache = self.options.cache.as_ref();
        let keys: Vec<_> = cache
            .map(|cache| {
                (0..blob_slots_of(&self.root, n))
                    .map(|i| cache.key(Slot { blob: n, index: i }.cache_id()))
                    .collect()
            })
            .unwrap_or_default();
        let value = (self.store.get(&blob_key(n)).await?)
            .ok_or_else(|| invalid("root: a blob is missing"))?;
        let blob = Arc::new(decode_blob(&value)?);
        if let Some(cache) = cache {
            for (key, slot) in keys.into_iter().zip(blob.iter()) {
                cache.insert_read(key, slot.clone());
            }
        }
        self.keep(n, blob.clone());
        Ok(blob)
    }

    fn keep(&self, n: u64, blob: Decoded) {
        let mut blobs = self.blobs.lock().unwrap();
        if blobs.len() >= KEPT_BLOBS {
            blobs.clear();
        }
        blobs.insert(n, blob);
    }

    pub(crate) fn check(&self, writes: &Writes, records: &Records) -> io::Result<()> {
        let paged = |id| matches!(self.root.directory[&id].kind, Kind::Paged { .. });
        for (name, value) in records {
            if value.len() > log::MAX_RECORD_BYTES {
                let (len, max) = (value.len(), log::MAX_RECORD_BYTES);
                return refuse(format!("record {name:?} of {len} bytes exceeds {max}"));
            }
        }
        for name in records.keys().chain(writes.keys()) {
            if name.len() > log::MAX_NAME_BYTES {
                return refuse(format!(
                    "name {name:?} exceeds {} bytes",
                    log::MAX_NAME_BYTES
                ));
            }
        }
        for (name, written) in writes {
            if records.contains_key(name) || self.root.file_id(name).is_some_and(|id| !paged(id)) {
                return refuse(format!("{name:?} is a record, not a paged file"));
            }
            let size = written.size;
            if !size.is_multiple_of(512) || size > log::MAX_DATABASE_BYTES {
                return refuse(format!("{name:?}: invalid size {size}"));
            }
            if written.pages.keys().any(|&page| page >= 1 << 32) {
                return refuse(format!("{name:?}: a page past the largest file"));
            }
        }
        for name in records.keys() {
            if self.root.file_id(name).is_some_and(paged) {
                return refuse(format!("{name:?} is a paged file, not a record"));
            }
        }
        Ok(())
    }

    /// Commit `writes` and `records`: see the module documentation.
    pub(crate) async fn commit(
        &self,
        mut writes: Writes,
        mut records: Records,
    ) -> io::Result<(FlushReport, Self)> {
        self.check(&writes, &records)?;
        records.retain(|name, value| {
            self.records
                .get(name)
                .map_or(!value.is_empty(), |v| v != value)
        });
        writes.retain(|name, written| {
            let current = self.file_size(name);
            !written.pages.is_empty() || written.size_after(current) != current
        });
        if writes.is_empty() && records.is_empty() {
            let after = self.continued(self.root.clone(), self.root_bytes.clone());
            return Ok((FlushReport::default(), after.registered()));
        }
        let cache = self.options.cache.as_ref();
        let mut root = self.root.clone();
        let allocate = |root: &mut Root, name: &str, kind: Kind| match root.file_id(name) {
            Some(id) => Ok(id),
            None if root.next_file >= log::MAX_FILES => refuse("too many files".into()),
            None => {
                let id = root.next_file;
                root.next_file += 1;
                let name = name.to_owned();
                root.directory.insert(id, Entry { name, kind });
                Ok(id)
            }
        };
        // What this flush's blobs hold.
        let mut fresh: Vec<(Target, Bytes)> = Vec::new();
        for (name, written) in writes {
            let base = self.bases.get(&name);
            let kind = Kind::Paged {
                size: 0,
                base: base.map(|base| base.id()),
                map: BTreeMap::new(),
            };
            let id = allocate(&mut root, &name, kind)?;
            let current = self.file_size(&name);
            let size = written.size_after(current);
            let Some(Entry {
                kind:
                    Kind::Paged {
                        size: held,
                        base: based,
                        map,
                    },
                ..
            }) = root.directory.get_mut(&id)
            else {
                unreachable!("checked: a paged file");
            };
            let based = based.is_some();
            let mut pages = written.pages;
            for page in size.div_ceil(PAGE)..current.div_ceil(PAGE) {
                pages.entry(page).or_insert(None);
            }
            for (page, image) in pages {
                let image = match (based, base) {
                    (true, None) if image.is_some() => {
                        return refuse(format!("{name:?} has a base: open it with it"));
                    }
                    (true, Some(base)) if image.as_deref() == base.page(page) => None,
                    _ => image,
                };
                map.remove(&page);
                if let Some(image) = image {
                    fresh.push((Target::Page(page_id(id, page)), image));
                }
            }
            *held = size;
        }
        let budget = self.options.inline_log_bytes;
        let mut after_records = self.records.clone();
        for (name, value) in records {
            let id = allocate(&mut root, &name, Kind::Record(Value::Inline(Bytes::new())))?;
            let inline = value.len() <= budget;
            if let Some(entry) = root.directory.get_mut(&id) {
                entry.kind = Kind::Record(Value::Inline(match inline {
                    true => value.clone(),
                    false => Bytes::new(),
                }));
            }
            if !inline {
                fresh.push((Target::Record(id), value.clone()));
            }
            after_records.insert(name, value);
        }
        if cache.is_some_and(|cache| !cache.claim(self.version())) {
            return Err(io::Error::other(
                "another state flushed since this one opened; reopen",
            ));
        }
        // Copy forward the live slots of blobs mostly dead.
        let live = root.live();
        let below = compact_below();
        let mut victims: Vec<(u32, u64)> = (live.iter())
            .filter(|&(n, &count)| f64::from(count) < below * f64::from(root.blobs[n]))
            .map(|(&n, &count)| (count, n))
            .collect();
        victims.sort();
        let mut moved = 0;
        victims.retain(|&(count, _)| {
            moved += count as usize;
            moved <= MAX_COMPACTED
        });
        let victims: BTreeSet<u64> = victims.into_iter().map(|(_, n)| n).collect();
        let mut compacted = 0;
        if !victims.is_empty() {
            // From this state's decoded blobs, else the page cache, else
            // the blobs themselves, fetched before anything is written.
            let moving: Vec<(Target, Slot)> = root
                .slots()
                .filter(|(_, slot)| victims.contains(&slot.blob))
                .collect();
            let mut found: Vec<Option<Bytes>> = Vec::with_capacity(moving.len());
            for (_, slot) in &moving {
                let held = self.blobs.lock().unwrap().get(&slot.blob).cloned();
                found.push(match (held, cache) {
                    (Some(blob), _) => Some(slot_in(&blob, *slot)?),
                    (None, Some(cache)) => cache.get(slot.cache_id()).await,
                    (None, None) => None,
                });
            }
            let missing: BTreeSet<u64> = (moving.iter().zip(&found))
                .filter(|(_, found)| found.is_none())
                .map(|((_, slot), _)| slot.blob)
                .collect();
            let keys: Vec<String> = missing.iter().map(|&n| blob_key(n)).collect();
            let values = try_join_all(keys.iter().map(|key| self.store.get(key))).await?;
            let mut decoded = HashMap::new();
            for (n, value) in missing.into_iter().zip(values) {
                let value = value.ok_or_else(|| invalid("root: a blob is missing"))?;
                decoded.insert(n, Arc::new(decode_blob(&value)?));
            }
            for ((target, slot), found) in moving.into_iter().zip(found) {
                let bytes = match found {
                    Some(bytes) => bytes,
                    None => slot_in(&decoded[&slot.blob], slot)?,
                };
                fresh.push((target, bytes));
                compacted += 1;
            }
        }
        // Assign slots: pages in order, then records, in blobs of at most
        // `blob_slots`.
        fresh.sort_by_key(|(target, _)| *target);
        let mut blobs = Vec::new();
        for chunk in fresh.chunks(blob_slots()) {
            let n = root.next_blob;
            root.next_blob += 1;
            root.blobs.insert(n, chunk.len() as u32);
            for (index, (target, _)) in chunk.iter().enumerate() {
                let slot = Slot {
                    blob: n,
                    index: index as u32,
                };
                match *target {
                    Target::Page(id) => {
                        if let Some(Entry {
                            kind: Kind::Paged { map, .. },
                            ..
                        }) = root.directory.get_mut(&file_of(id))
                        {
                            map.insert(page_of(id), slot);
                        }
                    }
                    Target::Record(id) => {
                        if let Some(entry) = root.directory.get_mut(&id) {
                            entry.kind = Kind::Record(Value::In(slot));
                        }
                    }
                }
            }
            let slots: Vec<Bytes> = chunk.iter().map(|(_, bytes)| bytes.clone()).collect();
            blobs.push((n, encode_blob(&slots)?, Arc::new(slots)));
        }
        let live = root.live();
        let dead: BTreeSet<u64> = (root.blobs.keys())
            .copied()
            .filter(|n| !live.contains_key(n))
            .collect();
        for n in &dead {
            root.blobs.remove(n);
        }
        drop(self.reader.lock().unwrap().take());
        // This view is not used again: only other states hold blobs back.
        let others_open = cache.is_some_and(|cache| cache.other_readers(root.version + 1));
        let write = |key: &str, value: Option<Bytes>| {
            let queued = match value {
                Some(value) => self.store.put(key, value),
                None => self.store.delete(key),
            };
            queued.map_err(|error| self.failed(error))
        };
        // 1. The durable root's garbage, unless another state may read it.
        let mut garbage = dead;
        match others_open {
            true => garbage.extend(&self.root.garbage),
            false => {
                for &n in &self.root.garbage {
                    write(&blob_key(n), None)?;
                }
            }
        }
        root.garbage = garbage;
        // 2. The blobs, then 3. the root that names them.
        let mut logged_bytes = 0;
        for (n, value, _) in &blobs {
            logged_bytes += value.len();
            write(&blob_key(*n), Some(value.clone()))?;
        }
        root.version += 1;
        let bytes = root.encode();
        let record_bytes = bytes.len();
        write(ROOT, Some(bytes.clone()))?;
        if let Err(error) = self.store.sync().await {
            return Err(self.failed(error));
        }
        let version = root.version;
        if let Some(cache) = cache {
            // Blobs are immutable: their slots stay valid in the cache.
            for (n, _, slots) in &blobs {
                for (index, slot) in slots.iter().enumerate() {
                    let id = Slot {
                        blob: *n,
                        index: index as u32,
                    }
                    .cache_id();
                    cache.insert_read(cache.key(id), slot.clone());
                }
            }
            cache.record(Some(bytes.clone()), 0, Some(version));
            let named = root.record_blobs();
            let values = (blobs.iter())
                .filter(|(n, ..)| named.contains(n))
                .map(|(n, value, _)| (*n, value.clone()))
                .collect();
            cache.keep_values(&named, values);
        }
        let mut after = self.continued(root, Some(bytes));
        after.records = after_records;
        for (n, _, slots) in blobs {
            after.keep(n, slots);
        }
        let report = FlushReport {
            checkpointed_pages: compacted,
            logged_bytes,
            record_bytes,
        };
        Ok((report, after.registered()))
    }

    fn failed(&self, error: io::Error) -> io::Error {
        if let Some(cache) = &self.options.cache {
            cache.forget();
        }
        error
    }

    fn continued(&self, root: Root, root_bytes: Option<Bytes>) -> Self {
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            root,
            root_bytes,
            records: self.records.clone(),
            bases: self.bases.clone(),
            unchanged: false,
            blobs: self.blobs.clone(),
            page: Mutex::default(),
            reader: Mutex::default(),
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
        with.bases.insert(name.to_owned(), base);
        Ok(with.registered())
    }

    fn registered(self) -> Self {
        let reader = (self.options.cache.as_ref()).map(|cache| cache.reader(self.version()));
        *self.reader.lock().unwrap() = reader;
        self
    }
}

fn blob_slots_of(root: &Root, n: u64) -> u32 {
    root.blobs.get(&n).copied().unwrap_or(0)
}

fn slot_in(blob: &[Bytes], slot: Slot) -> io::Result<Bytes> {
    (blob.get(slot.index as usize).cloned()).ok_or_else(|| invalid("root: a slot past its blob"))
}

fn slot_of(blobs: &HashMap<u64, Decoded>, slot: Slot) -> io::Result<Bytes> {
    let blob = blobs
        .get(&slot.blob)
        .ok_or_else(|| invalid("root: a blob is missing"))?;
    slot_in(blob, slot)
}

fn refuse<T>(message: String) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root once every page's slot is in a blob of its own run: the
    /// worst case of a map after many scattered updates.
    #[test]
    fn root_size_when_fragmented() {
        for pages in [700u64, 7_000, 70_000] {
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut map = BTreeMap::new();
            for page in 0..pages {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                // Blobs of the last few thousand flushes, a few slots each.
                let blob = 100_000 + seed % (pages / 2).max(1);
                map.insert(
                    page,
                    Slot {
                        blob,
                        index: (seed >> 40) as u32 % 4,
                    },
                );
            }
            let mut root = Root {
                version: 1 << 20,
                next_blob: 1 << 20,
                ..Root::default()
            };
            for slot in map.values() {
                root.blobs.insert(slot.blob, 4);
            }
            root.directory.insert(
                0,
                Entry {
                    name: "db".into(),
                    kind: Kind::Paged {
                        size: pages * 4096,
                        base: None,
                        map,
                    },
                },
            );
            let bytes = root.encode();
            assert_eq!(Root::decode(&bytes).unwrap(), root);
            println!("{pages} pages, fragmented: root {} bytes", bytes.len());
        }
    }
}

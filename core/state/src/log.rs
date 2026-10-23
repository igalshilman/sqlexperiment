//! The commit record, and the log it holds.
//!
//! `commit` is the one key whose write commits: a transaction is committed
//! once a commit record that holds it is durable. The record holds the log
//! since the last checkpoint: each committed transaction's changed pages, as
//! segments. A small transaction's segments are inline in the record, so a
//! commit is one put. A large one's are put at `log/<n>`, immutable keys, before
//! the record that names them. So are the inline ones once they outgrow
//! [`crate::StateOptions::inline_log_bytes`]: a commit seals them into one
//! stored value, and the record keeps only their names. Every commit rewrites
//! the record, so this keeps commits small while the log grows long between
//! checkpoints.
//!
//! The store keeps writes in issue order (see [`crate::kv`]), so a value
//! written before the record that names it is there whenever the record is.
//!
//! # Compression against the previous images
//!
//! A commit rewrites the same few pages as the commits before it, each a little
//! changed. A segment's images are one zstd frame whose dictionary (a raw
//! prefix) is the previous image of each of its pages: so a page costs about
//! what changed in it. The previous images are those of the [`Chain`]: the
//! latest image of each page in the log before the segment. The log starts at
//! the last checkpoint, so replay rebuilds the chain from nothing.
//!
//! Segment encoding: LEB128 varints, the entry count, then per entry in
//! ascending page order the gap from the previous entry's page (the page itself
//! for the first) shifted left once, its low bit the kind (0: a hole, 1: an
//! image); then one zstd frame, with a checksum, of the images in entry order
//! (none when every entry is a hole). A value at `log/<n>` is `SL15`, then the
//! segment count and each segment's length and bytes.
use crate::{
    PAGE_SIZE,
    format::{COMMIT_MAGIC, LOG_MAGIC, log_key},
    kv::KvStore,
    page_codec::Page,
    zstd,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

/// Largest segment.
pub const MAX_SEGMENT_BYTES: usize = 1024 * 1024;
/// Most entries of a segment: their images and the frame's overhead stay
/// below [`MAX_SEGMENT_BYTES`]. Larger transactions span several segments.
pub const MAX_SEGMENT_PAGES: usize = 240;
/// Most transactions, and segments of one, a stored record may hold.
const MAX_TRANSACTIONS: u64 = 1 << 20;
/// Bytes of a record's value.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of a record's name.
pub const MAX_NAME_BYTES: usize = 255;
/// Most files and records a commit record may name.
pub(crate) const MAX_FILES: u32 = 1 << 16;
const MAGIC: usize = 4;
/// Largest version, epoch or segment number a stored record may name, far
/// from overflow.
pub(crate) const MAX_SEQUENCE: u64 = 1 << 62;
/// Largest database size a stored record may name (16 TiB).
pub(crate) const MAX_DATABASE_BYTES: u64 = 1 << 44;

/// Logical page images by page id (see [`page_id`]); `None` is a hole.
pub(crate) type Images = BTreeMap<u64, Option<Page>>;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
}

/// The previous images a segment is compressed against: the latest image of
/// each page in the log before it, by page id. The log's own images are its
/// chain.
pub(crate) type Chain = Images;

/// The dictionary of a segment of `file`'s `pages` after `chain`: the
/// previous image of each one with an image, in order; where the log has
/// none, the page of the file's `base`, if any.
fn dictionary<'a>(
    chain: &Chain,
    file: u32,
    pages: impl Iterator<Item = &'a (u64, bool)>,
    base: Option<&dyn Base>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, _) in pages.filter(|(_, image)| *image) {
        match chain.get(&page_id(file, *index)) {
            Some(Some(previous)) => out.extend_from_slice(previous),
            // A hole of a file with a base is its base's page.
            Some(None) | None => {
                if let Some(page) = base.and_then(|base| base.page(*index)) {
                    out.extend_from_slice(page);
                }
            }
        }
    }
    out
}

/// What a paged file's absent pages hold: a golden image it diverges from (a
/// VM's memory, say). An absent page, or a hole, of a file with a base reads
/// as the base's page (zeros past it), and a page is compressed against it
/// the first time it is logged, and in place.
pub trait Base: Send + Sync {
    /// Names the base: a file made with one is opened only with it again.
    fn id(&self) -> u64;
    /// Page `index` (4096 bytes); `None` past the base's end.
    fn page(&self, index: u64) -> Option<&[u8]>;
}

/// The bases of files, by id.
pub(crate) type Bases = BTreeMap<u32, std::sync::Arc<dyn Base>>;

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
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

/// Encode a transaction's images as segments, each of one file's pages,
/// compressed against `chain` (and a file's base): each file's segments, by
/// file.
pub(crate) fn encode_transaction(
    images: &Images,
    chain: &Chain,
    bases: &Bases,
) -> io::Result<BTreeMap<u32, Vec<Bytes>>> {
    let mut files: BTreeMap<u32, Vec<(u64, &Option<Page>)>> = BTreeMap::new();
    for (&id, image) in images {
        files
            .entry(file_of(id))
            .or_default()
            .push((page_of(id), image));
    }
    let mut segments = BTreeMap::new();
    for (file, entries) in files {
        let base = bases.get(&file).map(|base| &**base);
        let encoded = (entries.chunks(MAX_SEGMENT_PAGES))
            .map(|entries| encode_segment(file, entries, chain, base).map(Bytes::from))
            .collect::<io::Result<_>>()?;
        segments.insert(file, encoded);
    }
    Ok(segments)
}

/// One segment of `file`'s `entries` (page numbers in the file).
fn encode_segment(
    file: u32,
    entries: &[(u64, &Option<Page>)],
    chain: &Chain,
    base: Option<&dyn Base>,
) -> io::Result<Vec<u8>> {
    let pages: Vec<(u64, bool)> = entries
        .iter()
        .map(|(index, image)| (*index, image.is_some()))
        .collect();
    let mut out = Vec::with_capacity(16 + 2 * entries.len());
    put_varint(&mut out, entries.len() as u64);
    let mut previous = None;
    for &(index, image) in &pages {
        let gap = previous.map_or(index, |p: u64| index - p - 1);
        put_varint(&mut out, gap << 1 | u64::from(image));
        previous = Some(index);
    }
    let raw: Vec<u8> = entries
        .iter()
        .filter_map(|(_, image)| image.as_deref())
        .flatten()
        .copied()
        .collect();
    if !raw.is_empty() {
        let dictionary = dictionary(chain, file, pages.iter(), base);
        zstd::compress(&raw, &dictionary, true, |frame| {
            out.reserve_exact(frame.len());
            out.extend_from_slice(frame);
        })?;
    }
    debug_assert!(out.len() + MAGIC <= MAX_SEGMENT_BYTES);
    Ok(out)
}

/// A segment's entries: each page, and whether it has an image (or is a
/// hole). The images' frame follows at the returned offset.
pub(crate) fn entries(bytes: &[u8]) -> io::Result<(Vec<(u64, bool)>, usize)> {
    let segment = || invalid("log segment");
    if bytes.len() > MAX_SEGMENT_BYTES {
        return Err(segment());
    }
    let mut at = 0;
    let count = get_varint(bytes, &mut at).ok_or_else(segment)?;
    if count > MAX_SEGMENT_PAGES as u64 {
        return Err(segment());
    }
    let mut pages: Vec<(u64, bool)> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let entry = get_varint(bytes, &mut at).ok_or_else(segment)?;
        let gap = entry >> 1;
        let index = match pages.last() {
            Some(&(last, _)) => last.checked_add(gap).and_then(|i| i.checked_add(1)),
            None => Some(gap),
        }
        .filter(|&index| index < MAX_DATABASE_BYTES / 4096)
        .ok_or_else(segment)?;
        pages.push((index, entry & 1 == 1));
    }
    let images = pages.iter().any(|(_, image)| *image);
    if images == (at == bytes.len()) {
        return Err(segment()); // a frame without images, or images without one
    }
    Ok((pages, at))
}

/// Decode a segment of `file`'s pages that follows `chain`: images by page
/// id.
pub(crate) fn decode_segment(
    bytes: &[u8],
    chain: &Chain,
    file: u32,
    base: Option<&dyn Base>,
) -> io::Result<Images> {
    let (pages, at) = entries(bytes)?;
    let count = pages.iter().filter(|(_, image)| *image).count();
    let mut images = Images::new();
    if count == 0 {
        images.extend(pages.iter().map(|&(index, _)| (page_id(file, index), None)));
        return Ok(images);
    }
    let dictionary = dictionary(chain, file, pages.iter(), base);
    let mut raw = Vec::with_capacity(count * PAGE_SIZE);
    zstd::decompress(&mut raw, &bytes[at..], &dictionary)
        .map_err(|_| invalid("log segment images"))?;
    if raw.len() != count * PAGE_SIZE {
        return Err(invalid("log segment images"));
    }
    let raw = Bytes::from(raw);
    let mut next = 0;
    for &(index, image) in &pages {
        let page = image.then(|| {
            next += PAGE_SIZE;
            raw.slice(next - PAGE_SIZE..next)
        });
        images.insert(page_id(file, index), page);
    }
    Ok(images)
}

/// Segments as a `log/<n>` value: the magic, the count, then each one's
/// length and bytes.
pub(crate) fn stored_value(segments: &[Bytes]) -> Bytes {
    let len = segments.iter().map(|s| s.len() + 4).sum::<usize>();
    let mut out = Vec::with_capacity(MAGIC + 4 + len);
    out.extend_from_slice(LOG_MAGIC);
    put_varint(&mut out, segments.len() as u64);
    for segment in segments {
        put_varint(&mut out, segment.len() as u64);
        out.extend_from_slice(segment);
    }
    out.into()
}

/// The segments of a `log/<n>` value: log segments, or a record's value.
pub(crate) fn stored_segments(bytes: &Bytes) -> io::Result<Vec<Bytes>> {
    let value = || invalid("log segment");
    if bytes.len() < MAGIC || &bytes[..MAGIC] != LOG_MAGIC {
        return Err(value());
    }
    let mut at = MAGIC;
    let count = get_varint(bytes, &mut at).ok_or_else(value)?;
    if count > MAX_TRANSACTIONS {
        return Err(value());
    }
    let mut segments = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        let len = get_varint(bytes, &mut at).ok_or_else(value)? as usize;
        let end = at.checked_add(len).filter(|&end| end <= bytes.len());
        let end = end.filter(|_| len <= MAX_RECORD_BYTES).ok_or_else(value)?;
        segments.push(bytes.slice(at..end));
        at = end;
    }
    if at != bytes.len() {
        return Err(value());
    }
    Ok(segments)
}

/// Where a segment of a committed transaction, or a record's value, is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    /// In the commit record.
    Inline(Bytes),
    /// At `log/<n>`, at this index among the value's segments.
    Stored(u64, u32),
}

/// A page's id across files: its file's id in the high 32 bits, its page
/// number in the low 32 (a file holds at most 2^32 pages). The cache, the
/// hint and the log's images key pages by it.
pub(crate) fn page_id(file: u32, page: u64) -> u64 {
    debug_assert!(page < 1 << 32);
    u64::from(file) << 32 | page
}

/// The file of a page id.
pub(crate) fn file_of(id: u64) -> u32 {
    (id >> 32) as u32
}

/// The page number of a page id, in its file.
pub(crate) fn page_of(id: u64) -> u64 {
    id & 0xffff_ffff
}

/// A file of the directory: its name, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub name: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    /// A whole value, replaced whole, kept by checkpoints.
    Record(Part),
    /// 4 KiB pages: the log since the last checkpoint holds its changed
    /// pages, its page keys the rest. Its size after the log, and the id of
    /// its base (see [`Base`]), if it has one.
    Paged { size: u64, base: Option<u64> },
}

/// One file's part of a committed transaction.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    pub file: u32,
    /// The file's size after it.
    pub size: u64,
    /// Its segments, each of this file's pages.
    pub parts: Vec<Part>,
}

/// A committed transaction, as the commit record holds it: each paged file
/// it changed, in ascending id order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Logged {
    pub changes: Vec<Change>,
}

/// The commit record at `commit`.
///
/// Encoding: `SC15`, then LEB128 varints: the version, the epoch, the next
/// segment number, the garbage's count and its numbers (ascending, each from
/// the one before), the next file id, the directory's count, and per file
/// its id (ascending, as the gap after the one before), its name's length and
/// bytes, then 0 and its part (a record), 1 and its size in 512-byte units (a
/// paged file), or 2, its size and its base's id (a paged file with a base);
/// then the transaction count, and per transaction its change count, per
/// change its file's id, its size in 512-byte units and its part count and
/// parts. A part is 0 and the bytes' length and bytes (inline), or 1, the
/// stored value's number and the part's index in it (stored).
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Commit {
    /// Transactions ever committed: the namespace's version.
    pub version: u64,
    /// Checkpoints ever recorded: page keys changed only under another epoch.
    pub epoch: u64,
    /// The number of the next stored segment.
    pub next_segment: u64,
    /// Stored values no transaction in the log names, to delete: those a
    /// checkpoint passed, and a seal not yet named.
    pub garbage: BTreeSet<u64>,
    /// The id the next new file takes: ids are never reused.
    pub next_file: u32,
    /// The files, by id: records (the host's values, each committed with the
    /// last transaction that set it, its outbox say) and paged files.
    pub directory: BTreeMap<u32, Entry>,
    /// The log since the last checkpoint, in order.
    pub log: Vec<Logged>,
}

/// Reads a commit record: each method fails on a malformed or implausible
/// value.
struct Reader<'a> {
    bytes: &'a Bytes,
    at: usize,
}

impl Reader<'_> {
    fn varint(&mut self) -> io::Result<u64> {
        get_varint(self.bytes, &mut self.at).ok_or_else(record_error)
    }

    /// A varint of at most `max`.
    fn bounded(&mut self, max: u64) -> io::Result<u64> {
        Some(self.varint()?)
            .filter(|&n| n <= max)
            .ok_or_else(record_error)
    }

    /// A size in 512-byte units, as bytes.
    fn size(&mut self) -> io::Result<u64> {
        let units = self.bounded(MAX_DATABASE_BYTES / 512)?;
        Ok(units * 512)
    }

    /// A length, then that many bytes, of at most `max`.
    fn bytes(&mut self, max: usize) -> io::Result<Bytes> {
        let len = self.bounded(max as u64)? as usize;
        let end = (self.at.checked_add(len))
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(record_error)?;
        let bytes = self.bytes.slice(self.at..end);
        self.at = end;
        Ok(bytes)
    }

    /// A part of at most `max` bytes inline, or naming a stored value below
    /// `next_segment` and not in `garbage`.
    fn part(&mut self, max: usize, next_segment: u64, garbage: &BTreeSet<u64>) -> io::Result<Part> {
        match self.varint()? {
            0 => Ok(Part::Inline(self.bytes(max)?)),
            1 => {
                let n = self.varint()?;
                let index = u32::try_from(self.varint()?).map_err(|_| record_error())?;
                if n >= next_segment || garbage.contains(&n) {
                    return Err(record_error());
                }
                Ok(Part::Stored(n, index))
            }
            _ => Err(record_error()),
        }
    }
}

impl Commit {
    pub(crate) fn encode(&self) -> Bytes {
        let inline: usize = self
            .parts()
            .map(|part| match part {
                Part::Inline(bytes) => bytes.len() + 4,
                Part::Stored(..) => 15,
            })
            .sum();
        let names: usize = self.directory.values().map(|e| e.name.len() + 8).sum();
        let mut out = Vec::with_capacity(64 + names + 8 * self.log.len() + inline);
        out.extend_from_slice(COMMIT_MAGIC);
        for n in [
            self.version,
            self.epoch,
            self.next_segment,
            self.garbage.len() as u64,
        ] {
            put_varint(&mut out, n);
        }
        let mut previous = 0;
        for &n in &self.garbage {
            put_varint(&mut out, n - previous);
            previous = n;
        }
        put_varint(&mut out, u64::from(self.next_file));
        put_varint(&mut out, self.directory.len() as u64);
        let mut previous = None;
        for (&id, entry) in &self.directory {
            put_varint(&mut out, previous.map_or(id, |p: u32| id - p - 1).into());
            previous = Some(id);
            put_varint(&mut out, entry.name.len() as u64);
            out.extend_from_slice(entry.name.as_bytes());
            match &entry.kind {
                Kind::Record(part) => {
                    put_varint(&mut out, 0);
                    put_part(&mut out, part);
                }
                Kind::Paged { size, base: None } => {
                    put_varint(&mut out, 1);
                    put_varint(&mut out, size / 512);
                }
                Kind::Paged {
                    size,
                    base: Some(base),
                } => {
                    put_varint(&mut out, 2);
                    put_varint(&mut out, size / 512);
                    put_varint(&mut out, *base);
                }
            }
        }
        put_varint(&mut out, self.log.len() as u64);
        for transaction in &self.log {
            put_varint(&mut out, transaction.changes.len() as u64);
            for change in &transaction.changes {
                put_varint(&mut out, change.file.into());
                put_varint(&mut out, change.size / 512);
                put_varint(&mut out, change.parts.len() as u64);
                for part in &change.parts {
                    put_part(&mut out, part);
                }
            }
        }
        out.into()
    }

    pub(crate) fn decode(bytes: &Bytes) -> io::Result<Self> {
        if bytes.len() < MAGIC || &bytes[..MAGIC] != COMMIT_MAGIC {
            return Err(record_error());
        }
        let mut r = Reader { bytes, at: MAGIC };
        let version = r.bounded(MAX_SEQUENCE)?;
        let epoch = r.bounded(MAX_SEQUENCE)?;
        let next_segment = r.bounded(MAX_SEQUENCE)?;
        let garbage_count = r.bounded(MAX_TRANSACTIONS)?;
        let mut garbage = BTreeSet::new();
        let mut previous = 0u64;
        for first in (0..garbage_count).map(|i| i == 0) {
            let delta = r.varint()?;
            // Ascending, and below the next segment.
            let n = previous.checked_add(delta).filter(|&n| n < next_segment);
            let n = n.filter(|_| first || delta > 0).ok_or_else(record_error)?;
            garbage.insert(n);
            previous = n;
        }
        let next_file = r.bounded(MAX_FILES.into())? as u32;
        let files = r.bounded(next_file.into())?;
        let mut directory = BTreeMap::new();
        let mut previous: Option<u32> = None;
        for _ in 0..files {
            let gap = r.varint()?;
            let id = match previous {
                Some(p) => u64::from(p)
                    .checked_add(gap)
                    .and_then(|id| id.checked_add(1)),
                None => Some(gap),
            };
            let id = (id.filter(|&id| id < u64::from(next_file))).ok_or_else(record_error)? as u32;
            previous = Some(id);
            let name = r.bytes(MAX_NAME_BYTES)?;
            let name = String::from_utf8(name.to_vec()).map_err(|_| record_error())?;
            let kind = match r.varint()? {
                0 => Kind::Record(r.part(MAX_RECORD_BYTES, next_segment, &garbage)?),
                1 => Kind::Paged {
                    size: r.size()?,
                    base: None,
                },
                2 => Kind::Paged {
                    size: r.size()?,
                    base: Some(r.varint()?),
                },
                _ => return Err(record_error()),
            };
            if directory.values().any(|entry: &Entry| entry.name == name) {
                return Err(record_error());
            }
            directory.insert(id, Entry { name, kind });
        }
        let paged = |file: u32| {
            matches!(
                directory.get(&file),
                Some(Entry {
                    kind: Kind::Paged { .. },
                    ..
                })
            )
        };
        let count = r.bounded(MAX_TRANSACTIONS.min(version))?;
        let mut log = Vec::with_capacity(count.min(1024) as usize);
        // The size each file has after the log, which the directory holds.
        let mut sizes = BTreeMap::new();
        for _ in 0..count {
            let changes_count = r.bounded(files)?;
            let mut changes = Vec::with_capacity(changes_count as usize);
            for _ in 0..changes_count {
                let file = r.bounded(u32::MAX.into())? as u32;
                // Distinct paged files, ascending.
                let ascending = changes.last().is_none_or(|last: &Change| last.file < file);
                if !paged(file) || !ascending {
                    return Err(record_error());
                }
                let size = r.size()?;
                let parts_count = r.bounded(MAX_TRANSACTIONS)?;
                let mut parts = Vec::with_capacity(parts_count.min(1024) as usize);
                for _ in 0..parts_count {
                    parts.push(r.part(MAX_SEGMENT_BYTES, next_segment, &garbage)?);
                }
                sizes.insert(file, size);
                changes.push(Change { file, size, parts });
            }
            log.push(Logged { changes });
        }
        let sized = sizes.iter().all(|(file, size)| {
            matches!(&directory[file].kind, Kind::Paged { size: held, .. } if held == size)
        });
        if r.at != bytes.len() || !sized {
            return Err(record_error());
        }
        Ok(Self {
            version,
            epoch,
            next_segment,
            garbage,
            next_file,
            directory,
            log,
        })
    }

    /// The log's parts, in order.
    fn log_parts(&self) -> impl Iterator<Item = &Part> {
        (self.log.iter())
            .flat_map(|t| &t.changes)
            .flat_map(|change| &change.parts)
    }

    /// Every part: the log's, then the records'.
    pub(crate) fn parts(&self) -> impl Iterator<Item = &Part> {
        let records = self
            .directory
            .values()
            .filter_map(|entry| match &entry.kind {
                Kind::Record(part) => Some(part),
                Kind::Paged { .. } => None,
            });
        self.log_parts().chain(records)
    }

    /// The id of the file named `name`.
    pub(crate) fn file_id(&self, name: &str) -> Option<u32> {
        let mut entries = self.directory.iter();
        entries
            .find(|(_, entry)| entry.name == name)
            .map(|(&id, _)| id)
    }

    /// The size of the paged file `file`; 0 for any other.
    pub(crate) fn size(&self, file: u32) -> u64 {
        match self.directory.get(&file).map(|entry| &entry.kind) {
            Some(Kind::Paged { size, .. }) => *size,
            _ => 0,
        }
    }

    /// The paged files, and their sizes.
    pub(crate) fn paged(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        (self.directory.iter()).filter_map(|(&id, entry)| match entry.kind {
            Kind::Paged { size, .. } => Some((id, size)),
            Kind::Record(_) => None,
        })
    }

    /// Bytes of the log's inline parts.
    pub(crate) fn inline_bytes(&self) -> usize {
        self.log_parts()
            .map(|part| match part {
                Part::Inline(bytes) => bytes.len(),
                Part::Stored(..) => 0,
            })
            .sum()
    }

    /// The pages the log's inline segments change, as page ids, read from
    /// their entries without decoding images: the log overrides them, so
    /// open need not fetch their page keys.
    pub(crate) fn inline_pages(&self) -> BTreeSet<u64> {
        let mut pages = BTreeSet::new();
        for change in self.log.iter().flat_map(|t| &t.changes) {
            for part in &change.parts {
                if let Part::Inline(bytes) = part
                    && let Ok((entries, _)) = entries(bytes)
                {
                    pages.extend(entries.iter().map(|&(page, _)| page_id(change.file, page)));
                }
            }
        }
        pages
    }

    /// Name the stored value `n` in place of the log's first `count` inline
    /// parts, which it holds.
    pub(crate) fn name(&mut self, n: u64, count: usize) {
        let inline = |part: &&mut Part| matches!(part, Part::Inline(_));
        let log = (self.log.iter_mut())
            .flat_map(|t| &mut t.changes)
            .flat_map(|change| &mut change.parts);
        for (index, part) in log.filter(inline).take(count).enumerate() {
            *part = Part::Stored(n, index as u32);
        }
    }

    /// The log's inline parts, in order.
    pub(crate) fn inline_parts(&self) -> Vec<Bytes> {
        self.log_parts()
            .filter_map(|part| match part {
                Part::Inline(bytes) => Some(bytes.clone()),
                Part::Stored(..) => None,
            })
            .collect()
    }

    /// The stored values the record names, by number.
    pub(crate) fn stored_values(&self) -> BTreeSet<u64> {
        self.parts()
            .filter_map(|part| match part {
                Part::Stored(n, _) => Some(*n),
                Part::Inline(_) => None,
            })
            .collect()
    }
}

fn record_error() -> io::Error {
    invalid("commit record")
}

fn put_part(out: &mut Vec<u8>, part: &Part) {
    match part {
        Part::Inline(bytes) => {
            put_varint(out, 0);
            put_varint(out, bytes.len() as u64);
            out.extend_from_slice(bytes);
        }
        Part::Stored(n, index) => {
            put_varint(out, 1);
            put_varint(out, *n);
            put_varint(out, u64::from(*index));
        }
    }
}

/// The log as replayed from a commit record.
#[derive(Clone)]
pub(crate) struct Log {
    /// Committed transactions in the log.
    pub transactions: usize,
    /// Latest committed image of every page the log changes: what the next
    /// transaction is compressed against.
    pub images: Images,
    /// Encoded bytes of the log's segments.
    pub bytes: usize,
    /// The segments of each file with a base not yet given, in log order:
    /// they decode against it, once it is (see [`Self::decode_file`]).
    pub undecoded: BTreeMap<u32, Vec<Bytes>>,
}

impl Log {
    pub(crate) fn new() -> Self {
        Self {
            transactions: 0,
            images: Images::new(),
            bytes: 0,
            undecoded: BTreeMap::new(),
        }
    }

    /// Whether `file` has undecoded segments whose images the log still
    /// holds: a later hole of every one of their pages would make them moot.
    pub(crate) fn needs_base(&self, file: u32) -> bool {
        let Some(segments) = self.undecoded.get(&file) else {
            return false;
        };
        segments.iter().any(|segment| {
            entries(segment).is_ok_and(|(pages, _)| {
                (pages.iter())
                    .any(|&(page, image)| image && !self.images.contains_key(&page_id(file, page)))
            })
        })
    }

    /// Whether some file's segments wait for their base: a checkpoint, which
    /// writes every page's latest image, waits too.
    pub(crate) fn waits_for_bases(&self) -> bool {
        self.undecoded.keys().any(|&file| self.needs_base(file))
    }

    /// Decode `file`'s segments against `base`, now given. Pages a later
    /// transaction changed keep that change.
    pub(crate) fn decode_file(&mut self, file: u32, base: &dyn Base) -> io::Result<()> {
        let Some(segments) = self.undecoded.remove(&file) else {
            return Ok(());
        };
        let mut chain = Chain::default();
        for segment in &segments {
            let images = decode_segment(segment, &chain, file, Some(base))?;
            chain.extend(images);
        }
        for (page, image) in chain {
            self.images.entry(page).or_insert(image);
        }
        Ok(())
    }

    /// Add a committed transaction.
    pub(crate) fn append(&mut self, images: Images, bytes: usize) {
        self.transactions += 1;
        self.images.extend(images);
        self.bytes += bytes;
    }
}

/// Stored values by number, as `log/<n>` holds them.
pub(crate) type Values = BTreeMap<u64, Bytes>;

/// Records' values by name.
pub(crate) type Records = BTreeMap<String, Bytes>;

/// What a commit record holds, read: the log replayed, the records' values,
/// and every stored value the record names.
pub(crate) struct Replayed {
    pub log: Log,
    pub records: Records,
    pub values: Values,
}

/// Replay `commit`'s log and read its records: the inline parts, and the stored
/// values it names, those not in `known` read together. Every segment decodes
/// in order against the ones before it; a file with a base waits for it.
pub(crate) async fn replay(
    store: &dyn KvStore,
    commit: &Commit,
    mut known: Values,
) -> io::Result<Replayed> {
    let named = commit.stored_values();
    known.retain(|n, _| named.contains(n));
    let missing: Vec<u64> = named
        .iter()
        .filter(|n| !known.contains_key(n))
        .copied()
        .collect();
    known.extend(
        missing
            .iter()
            .copied()
            .zip(read_values(store, &missing).await?),
    );
    let mut segments = BTreeMap::new();
    for (&n, value) in &known {
        segments.insert(n, stored_segments(value)?);
    }
    let resolve = |part: &Part| match part {
        Part::Inline(bytes) => Ok(bytes.clone()),
        Part::Stored(n, index) => segments
            .get(n)
            .and_then(|value: &Vec<Bytes>| value.get(*index as usize))
            .cloned()
            .ok_or_else(|| invalid("log segment")),
    };
    let mut log = Log::new();
    // A file with a base decodes once it is opened with it.
    let based: BTreeSet<u32> = (commit.directory.iter())
        .filter(|(_, entry)| matches!(entry.kind, Kind::Paged { base: Some(_), .. }))
        .map(|(&id, _)| id)
        .collect();
    for transaction in &commit.log {
        let mut images = Images::new();
        let mut bytes = 0;
        for change in &transaction.changes {
            for part in &change.parts {
                let segment = resolve(part)?;
                bytes += segment.len();
                if based.contains(&change.file) {
                    entries(&segment)?; // malformed fails open, as decoding would
                    log.undecoded.entry(change.file).or_default().push(segment);
                    continue;
                }
                // The parts of a transaction decode against the log before it.
                images.extend(decode_segment(&segment, &log.images, change.file, None)?);
            }
        }
        log.append(images, bytes);
    }
    let mut records = Records::new();
    for entry in commit.directory.values() {
        if let Kind::Record(part) = &entry.kind {
            records.insert(entry.name.clone(), resolve(part)?);
        }
    }
    Ok(Replayed {
        log,
        records,
        values: known,
    })
}

/// The values at `log/<n>` for each of `numbers`, read together.
async fn read_values(store: &dyn KvStore, numbers: &[u64]) -> io::Result<Vec<Bytes>> {
    let keys: Vec<_> = numbers.iter().map(|&n| log_key(n)).collect();
    let values = try_join_all(keys.iter().map(|key| store.get(key))).await?;
    values
        .into_iter()
        .map(|value| value.ok_or_else(|| invalid("missing log segment")))
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/log.rs"]
mod tests;

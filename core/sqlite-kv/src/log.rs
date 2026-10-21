//! The commit record, and the log it holds.
//!
//! `commit` is the one key whose write commits: a transaction is committed
//! once a commit record that holds it is durable. The record holds the log
//! since the last checkpoint: each committed transaction's changed pages, as
//! segments. A small transaction's segments are inline in the record, so a
//! save is one put. A large one's are put at `log/<n>`, immutable keys, before
//! the record that names them. So are the inline ones once they outgrow
//! [`crate::StorageOptions::inline_log_bytes`]: a commit seals them into one
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
    format::{COMMIT_MAGIC, LOG_MAGIC, log_key},
    io::BLOCK_SIZE,
    kv::KvStore,
    page_codec::Page,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};
use zstd_safe::{CCtx, CParameter, DCtx};

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
/// Most records a commit record may name.
pub(crate) const MAX_FILES: u32 = 1 << 16;
/// Bytes of a value in the host's chain.
pub const MAX_CHAIN_VALUE_BYTES: usize = 16 * 1024 * 1024;
const MAGIC: usize = 4;
const ZSTD_LEVEL: i32 = 3;
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
/// each page in the log before it, by page id.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chain {
    pub images: Images,
}

impl Chain {
    /// The dictionary of a segment of `file`'s `pages`: the previous image of
    /// each one with an image, in order.
    fn dictionary<'a>(&self, file: u32, pages: impl Iterator<Item = &'a (u64, bool)>) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, _) in pages.filter(|(_, image)| *image) {
            if let Some(Some(previous)) = self.images.get(&page_id(file, *index)) {
                out.extend_from_slice(previous);
            }
        }
        out
    }
}

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
/// compressed against `chain`: each file's segments, by file.
pub(crate) fn encode_transaction(
    images: &Images,
    chain: &Chain,
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
        let encoded = (entries.chunks(MAX_SEGMENT_PAGES))
            .map(|entries| encode_segment(file, entries, chain).map(Bytes::from))
            .collect::<io::Result<_>>()?;
        segments.insert(file, encoded);
    }
    Ok(segments)
}

/// One segment of `file`'s `entries` (page numbers in the file).
pub(crate) fn encode_segment(
    file: u32,
    entries: &[(u64, &Option<Page>)],
    chain: &Chain,
) -> io::Result<Vec<u8>> {
    let zstd = |e: zstd_safe::ErrorCode| io::Error::other(zstd_safe::get_error_name(e));
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
        let dictionary = chain.dictionary(file, pages.iter());
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(raw.len()));
        let mut cctx = CCtx::create();
        cctx.set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
            .map_err(zstd)?;
        cctx.set_parameter(CParameter::ChecksumFlag(true))
            .map_err(zstd)?;
        cctx.ref_prefix(&dictionary).map_err(zstd)?;
        cctx.compress2(&mut frame, &raw).map_err(zstd)?;
        out.extend_from_slice(&frame);
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
pub(crate) fn decode_segment(bytes: &[u8], chain: &Chain, file: u32) -> io::Result<Images> {
    let (pages, at) = entries(bytes)?;
    let count = pages.iter().filter(|(_, image)| *image).count();
    let mut images = Images::new();
    if count == 0 {
        images.extend(pages.iter().map(|&(index, _)| (page_id(file, index), None)));
        return Ok(images);
    }
    let dictionary = chain.dictionary(file, pages.iter());
    let mut raw = Vec::with_capacity(count * BLOCK_SIZE);
    let mut dctx = DCtx::create();
    dctx.ref_prefix(&dictionary)
        .and_then(|_| dctx.decompress(&mut raw, &bytes[at..]))
        .map_err(|_| invalid("log segment images"))?;
    if raw.len() != count * BLOCK_SIZE {
        return Err(invalid("log segment images"));
    }
    let raw = Bytes::from(raw);
    let mut next = 0;
    for &(index, image) in &pages {
        let page = image.then(|| {
            next += BLOCK_SIZE;
            raw.slice(next - BLOCK_SIZE..next)
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

/// The segments of a `log/<n>` value: log segments, or values of the host's
/// chain.
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
        let end = end
            .filter(|_| len <= MAX_CHAIN_VALUE_BYTES)
            .ok_or_else(value)?;
        segments.push(bytes.slice(at..end));
        at = end;
    }
    if at != bytes.len() {
        return Err(value());
    }
    Ok(segments)
}

/// A stored value holding a record's first inline parts: `log` of the log's,
/// then `chain` of the chain's.
#[derive(Clone, Debug)]
pub(crate) struct Sealed {
    pub n: u64,
    pub log: usize,
    pub chain: usize,
}

/// Where a segment of a committed transaction, or a value of the host's
/// chain, is.
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
    /// pages, its page keys the rest. Its size after the log.
    Paged { size: u64 },
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
/// bytes, then 0 and its part (a record) or 1 and its size in 512-byte units
/// (a paged file); then the transaction count, and per transaction its change
/// count, per change its file's id, its size in 512-byte units and its part
/// count and parts; then the chain's part count and parts. A part is 0 and
/// the bytes' length and bytes (inline), or 1, the stored value's number and
/// the part's index in it (stored).
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
    /// The host's chain: opaque values it appends to, or restarts, with its
    /// commits (a VM's page batches, say). Checkpoints keep it.
    pub chain: Vec<Part>,
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
                Kind::Paged { size } => {
                    put_varint(&mut out, 1);
                    put_varint(&mut out, size / 512);
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
        put_varint(&mut out, self.chain.len() as u64);
        for part in &self.chain {
            put_part(&mut out, part);
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
                1 => Kind::Paged { size: r.size()? },
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
        let chain_count = r.bounded(MAX_TRANSACTIONS)?;
        let mut chain = Vec::with_capacity(chain_count.min(1024) as usize);
        for _ in 0..chain_count {
            chain.push(r.part(MAX_CHAIN_VALUE_BYTES, next_segment, &garbage)?);
        }
        let sized = sizes.iter().all(|(file, size)| {
            matches!(&directory[file].kind, Kind::Paged { size: held } if held == size)
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
            chain,
        })
    }

    /// The parts a seal takes, in order: the log's then the chain's.
    fn sealable(&self) -> impl Iterator<Item = &Part> {
        self.log_parts().chain(&self.chain)
    }

    /// The log's parts, in order.
    fn log_parts(&self) -> impl Iterator<Item = &Part> {
        (self.log.iter())
            .flat_map(|t| &t.changes)
            .flat_map(|change| &change.parts)
    }

    /// Every part: the sealable ones, then the records'.
    pub(crate) fn parts(&self) -> impl Iterator<Item = &Part> {
        let records = self
            .directory
            .values()
            .filter_map(|entry| match &entry.kind {
                Kind::Record(part) => Some(part),
                Kind::Paged { .. } => None,
            });
        self.sealable().chain(records)
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
            Some(Kind::Paged { size }) => *size,
            _ => 0,
        }
    }

    /// The paged files, and their sizes.
    pub(crate) fn paged(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        (self.directory.iter()).filter_map(|(&id, entry)| match entry.kind {
            Kind::Paged { size } => Some((id, size)),
            Kind::Record(_) => None,
        })
    }

    /// Bytes of the inline parts, the log's and the chain's.
    pub(crate) fn inline_bytes(&self) -> usize {
        self.sealable()
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

    /// Name `sealed` in place of the inline parts it holds.
    pub(crate) fn name(&mut self, sealed: &Sealed) {
        let inline = |part: &&mut Part| matches!(part, Part::Inline(_));
        let log = (self.log.iter_mut())
            .flat_map(|t| &mut t.changes)
            .flat_map(|change| &mut change.parts);
        let log = log.filter(inline).take(sealed.log);
        let chain = self.chain.iter_mut().filter(inline).take(sealed.chain);
        for (index, part) in log.chain(chain).enumerate() {
            *part = Part::Stored(sealed.n, index as u32);
        }
    }

    /// The inline parts, the log's and then the chain's.
    pub(crate) fn inline_parts(&self) -> (Vec<Bytes>, Vec<Bytes>) {
        let inline = |part: &Part| match part {
            Part::Inline(bytes) => Some(bytes.clone()),
            Part::Stored(..) => None,
        };
        (
            self.log_parts().filter_map(inline).collect(),
            self.chain.iter().filter_map(inline).collect(),
        )
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

/// A committed transaction in the replayed log.
pub(crate) struct Transaction {
    /// Pages it changed, by id.
    pub pages: Vec<u64>,
    /// Encoded bytes of its segments.
    pub bytes: usize,
}

/// The log as replayed from a commit record.
pub(crate) struct Log {
    pub transactions: Vec<Transaction>,
    /// Latest committed image of every page the log changes.
    pub images: Images,
    /// Encoded bytes of the log's segments.
    pub bytes: usize,
    /// What the next transaction is compressed against.
    pub chain: Chain,
}

impl Log {
    pub(crate) fn new() -> Self {
        Self {
            transactions: Vec::new(),
            images: Images::new(),
            bytes: 0,
            chain: Chain::default(),
        }
    }

    /// Add a committed transaction.
    pub(crate) fn append(&mut self, images: Images, bytes: usize) {
        self.transactions.push(Transaction {
            pages: images.keys().copied().collect(),
            bytes,
        });
        self.chain.images.extend(images.clone());
        self.images.extend(images);
        self.bytes += bytes;
    }
}

/// Replay the log a commit record holds: its stored segments are read
/// together, then every segment decodes in order against the ones before it.
/// Stored values by number, as `log/<n>` holds them.
pub(crate) type Values = BTreeMap<u64, Bytes>;

/// Records' values by name.
pub(crate) type Records = BTreeMap<String, Bytes>;

/// What a commit record holds, read: the log replayed, the chain's values,
/// the records' values, and every stored value the record names.
pub(crate) struct Replayed {
    pub log: Log,
    pub chain: Vec<Bytes>,
    pub records: Records,
    pub values: Values,
}

/// Replay `commit`'s log and read its chain and records: the inline parts, and the stored
/// values it names, those not in `known` read together.
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
    for transaction in &commit.log {
        let mut images = Images::new();
        let mut bytes = 0;
        for change in &transaction.changes {
            for part in &change.parts {
                let segment = resolve(part)?;
                bytes += segment.len();
                // The parts of a transaction decode against the log before it.
                images.extend(decode_segment(&segment, &log.chain, change.file)?);
            }
        }
        log.append(images, bytes);
    }
    let chain = commit
        .chain
        .iter()
        .map(resolve)
        .collect::<io::Result<_>>()?;
    let mut records = Records::new();
    for entry in commit.directory.values() {
        if let Kind::Record(part) = &entry.kind {
            records.insert(entry.name.clone(), resolve(part)?);
        }
    }
    Ok(Replayed {
        log,
        chain,
        records,
        values: known,
    })
}

/// The values at `log/<n>` for each of `numbers`, read together.
pub(crate) async fn read_values(store: &dyn KvStore, numbers: &[u64]) -> io::Result<Vec<Bytes>> {
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

//! The commit record, and the log it holds.
//!
//! `commit` is the one key whose write commits: a transaction is committed
//! once a commit record that holds it is durable. The record holds the log
//! since the last checkpoint: each committed transaction's changed pages, as
//! segments. A small transaction's segments are inline in the record, so a
//! save is one put. A large one's are put first at `log/<n>`, immutable keys,
//! and the record names them once they are durable. So are the inline ones
//! once they outgrow [`crate::StorageOptions::inline_log_bytes`]: a commit
//! seals them, with its own, into one stored value, and the record keeps
//! only their names. Every commit rewrites the record, so this keeps commits
//! small while the log grows long between checkpoints.
//!
//! Nothing here relies on the order in which the store applies writes. A
//! write is relied on only once it is acknowledged: the segments a record
//! names before the record, the pages a checkpoint writes before the record
//! that moves past them.
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
//! (none when every entry is a hole). A value at `log/<n>` is `SL14`, then the
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
/// Bytes of a commit record's effects.
pub const MAX_EFFECTS_BYTES: usize = 1024 * 1024;
const MAGIC: usize = 4;
const ZSTD_LEVEL: i32 = 3;
/// Largest version, epoch or segment number a stored record may name, far
/// from overflow.
pub(crate) const MAX_SEQUENCE: u64 = 1 << 62;
/// Largest database size a stored record may name (16 TiB).
pub(crate) const MAX_DATABASE_BYTES: u64 = 1 << 44;

/// Whether a stored database size is plausible: whole pages of at least the
/// smallest SQLite page size, bounded.
fn valid_size(size: u64) -> bool {
    size.is_multiple_of(512) && size <= MAX_DATABASE_BYTES
}

/// Logical page images by page number; `None` is a hole.
pub(crate) type Images = BTreeMap<u64, Option<Page>>;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
}

/// The previous images a segment is compressed against: the latest image of
/// each page in the log before it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chain {
    pub images: Images,
}

impl Chain {
    /// The dictionary of a segment of `pages`: the previous image of each one
    /// with an image, in order.
    fn dictionary<'a>(&self, pages: impl Iterator<Item = &'a (u64, bool)>) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, _) in pages.filter(|(_, image)| *image) {
            if let Some(Some(previous)) = self.images.get(index) {
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

/// Encode a transaction's images as segments, compressed against `chain`.
/// An empty transaction (one [`crate::Session::commit`] records) has none.
pub(crate) fn encode_transaction(images: &Images, chain: &Chain) -> io::Result<Vec<Bytes>> {
    let entries: Vec<_> = images
        .iter()
        .map(|(&index, image)| (index, image))
        .collect();
    entries
        .chunks(MAX_SEGMENT_PAGES)
        .map(|entries| encode_segment(entries, chain).map(Bytes::from))
        .collect()
}

/// One segment of `entries`.
pub(crate) fn encode_segment(
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
        let dictionary = chain.dictionary(pages.iter());
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

/// Decode a segment that follows `chain`.
pub(crate) fn decode_segment(bytes: &[u8], chain: &Chain) -> io::Result<Images> {
    let (pages, at) = entries(bytes)?;
    let count = pages.iter().filter(|(_, image)| *image).count();
    let mut images = Images::new();
    if count == 0 {
        images.extend(pages.iter().map(|&(index, _)| (index, None)));
        return Ok(images);
    }
    let dictionary = chain.dictionary(pages.iter());
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
        images.insert(index, page);
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

/// The segments of a `log/<n>` value.
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
        let end = end.filter(|_| len <= MAX_SEGMENT_BYTES).ok_or_else(value)?;
        segments.push(bytes.slice(at..end));
        at = end;
    }
    if at != bytes.len() {
        return Err(value());
    }
    Ok(segments)
}

/// A stored value holding the first `segments` inline segments of a record's
/// log, acknowledged, which the record lists as garbage until the next names
/// it.
#[derive(Clone, Debug)]
pub(crate) struct Sealed {
    pub n: u64,
    pub value: Bytes,
    pub segments: usize,
}

/// Where a segment of a committed transaction is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    /// In the commit record.
    Inline(Bytes),
    /// At `log/<n>`, at this index among the value's segments.
    Stored(u64, u32),
}

/// A committed transaction, as the commit record holds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Logged {
    /// Database size after it.
    pub size: u64,
    pub parts: Vec<Part>,
}

/// The commit record at `commit`.
///
/// Encoding: `SC14`, then LEB128 varints: the version, the epoch, the
/// database size in 512-byte units, the next segment number, the garbage's
/// count and its numbers (ascending, each from the one before), the
/// effects' length and bytes, and the
/// transaction count; per transaction its
/// size in 512-byte units and its part count; per part 0 and the segment's
/// length and bytes (inline), or 1, the stored value's number and the
/// segment's index in it (stored).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Commit {
    /// Transactions ever committed: the database's version.
    pub version: u64,
    /// Checkpoints ever recorded: page keys changed only under another epoch.
    pub epoch: u64,
    /// Database size after the log.
    pub size: u64,
    /// The number of the next stored segment.
    pub next_segment: u64,
    /// Stored values no transaction in the log names, to delete: those a
    /// checkpoint passed, and a seal not yet named.
    pub garbage: BTreeSet<u64>,
    /// The host's bytes, committed with the last transaction that set them
    /// (its outbox, say): opaque here, and kept by checkpoints.
    pub effects: Bytes,
    /// The log since the last checkpoint, in order.
    pub log: Vec<Logged>,
}

impl Default for Commit {
    /// An empty database, never committed.
    fn default() -> Self {
        Self {
            version: 0,
            epoch: 0,
            size: 0,
            next_segment: 0,
            garbage: BTreeSet::new(),
            effects: Bytes::new(),
            log: Vec::new(),
        }
    }
}

impl Commit {
    pub(crate) fn encode(&self) -> Bytes {
        let inline: usize = self
            .log
            .iter()
            .flat_map(|t| &t.parts)
            .map(|part| match part {
                Part::Inline(bytes) => bytes.len() + 4,
                Part::Stored(..) => 15,
            })
            .sum();
        let mut out = Vec::with_capacity(64 + self.effects.len() + 4 * self.log.len() + inline);
        out.extend_from_slice(COMMIT_MAGIC);
        for n in [
            self.version,
            self.epoch,
            self.size / 512,
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
        put_varint(&mut out, self.effects.len() as u64);
        out.extend_from_slice(&self.effects);
        put_varint(&mut out, self.log.len() as u64);
        for transaction in &self.log {
            put_varint(&mut out, transaction.size / 512);
            put_varint(&mut out, transaction.parts.len() as u64);
            for part in &transaction.parts {
                match part {
                    Part::Inline(bytes) => {
                        put_varint(&mut out, 0);
                        put_varint(&mut out, bytes.len() as u64);
                        out.extend_from_slice(bytes);
                    }
                    Part::Stored(n, index) => {
                        put_varint(&mut out, 1);
                        put_varint(&mut out, *n);
                        put_varint(&mut out, u64::from(*index));
                    }
                }
            }
        }
        out.into()
    }

    pub(crate) fn decode(bytes: &Bytes) -> io::Result<Self> {
        let record = || invalid("commit record");
        if bytes.len() < MAGIC || &bytes[..MAGIC] != COMMIT_MAGIC {
            return Err(record());
        }
        let mut at = MAGIC;
        let varint = |at: &mut usize| get_varint(bytes, at).ok_or_else(record);
        let version = varint(&mut at)?;
        let epoch = varint(&mut at)?;
        let size = varint(&mut at)?.checked_mul(512).ok_or_else(record)?;
        let next_segment = varint(&mut at)?;
        let garbage_count = varint(&mut at)?;
        if garbage_count > MAX_TRANSACTIONS || next_segment > MAX_SEQUENCE {
            return Err(record());
        }
        let mut garbage = BTreeSet::new();
        let mut previous = 0u64;
        for first in (0..garbage_count).map(|i| i == 0) {
            let delta = varint(&mut at)?;
            // Ascending, and below the next segment.
            let n = previous.checked_add(delta).filter(|&n| n < next_segment);
            let n = n.filter(|_| first || delta > 0).ok_or_else(record)?;
            garbage.insert(n);
            previous = n;
        }
        let len = varint(&mut at)? as usize;
        let end = at.checked_add(len).filter(|&end| end <= bytes.len());
        let end = end
            .filter(|_| len <= MAX_EFFECTS_BYTES)
            .ok_or_else(record)?;
        let effects = bytes.slice(at..end);
        at = end;
        let count = varint(&mut at)?;
        let plausible = [version, epoch, next_segment]
            .iter()
            .all(|&n| n <= MAX_SEQUENCE)
            && valid_size(size)
            && count <= MAX_TRANSACTIONS
            && count <= version;
        if !plausible {
            return Err(record());
        }
        let mut log = Vec::with_capacity(count.min(1024) as usize);
        for _ in 0..count {
            let size = varint(&mut at)?.checked_mul(512).ok_or_else(record)?;
            let parts_count = varint(&mut at)?;
            if !valid_size(size) || parts_count > MAX_TRANSACTIONS {
                return Err(record());
            }
            let mut parts = Vec::with_capacity(parts_count.min(1024) as usize);
            for _ in 0..parts_count {
                parts.push(match varint(&mut at)? {
                    0 => {
                        let len = varint(&mut at)? as usize;
                        let end = at.checked_add(len).filter(|&end| end <= bytes.len());
                        let end = end
                            .filter(|_| len <= MAX_SEGMENT_BYTES)
                            .ok_or_else(record)?;
                        let part = Part::Inline(bytes.slice(at..end));
                        at = end;
                        part
                    }
                    1 => {
                        let n = varint(&mut at)?;
                        let index = u32::try_from(varint(&mut at)?).map_err(|_| record())?;
                        if n >= next_segment || garbage.contains(&n) {
                            return Err(record());
                        }
                        Part::Stored(n, index)
                    }
                    _ => return Err(record()),
                });
            }
            log.push(Logged { size, parts });
        }
        if at != bytes.len() || log.last().is_some_and(|last| last.size != size) {
            return Err(record());
        }
        Ok(Self {
            version,
            epoch,
            size,
            next_segment,
            garbage,
            effects,
            log,
        })
    }

    /// Bytes of the log's inline segments.
    pub(crate) fn inline_bytes(&self) -> usize {
        self.log
            .iter()
            .flat_map(|t| &t.parts)
            .map(|part| match part {
                Part::Inline(bytes) => bytes.len(),
                Part::Stored(..) => 0,
            })
            .sum()
    }

    /// The pages the log's inline segments change, read from their entries
    /// without decoding images: the log overrides them, so open need not
    /// fetch their page keys.
    pub(crate) fn inline_pages(&self) -> std::collections::BTreeSet<u64> {
        self.log
            .iter()
            .flat_map(|t| &t.parts)
            .filter_map(|part| match part {
                Part::Inline(bytes) => entries(bytes).ok(),
                Part::Stored(..) => None,
            })
            .flat_map(|(pages, _)| pages.into_iter().map(|(index, _)| index))
            .collect()
    }

    /// Name `sealed`, acknowledged, in place of the inline segments it holds,
    /// and take it out of the garbage.
    pub(crate) fn name(&mut self, sealed: &Sealed) {
        let parts = self.log.iter_mut().flat_map(|t| &mut t.parts);
        let inline = parts.filter(|part| matches!(part, Part::Inline(_)));
        for (index, part) in inline.take(sealed.segments).enumerate() {
            *part = Part::Stored(sealed.n, index as u32);
        }
        self.garbage.remove(&sealed.n);
    }

    /// The stored values the log names, by number.
    pub(crate) fn stored_values(&self) -> std::collections::BTreeSet<u64> {
        self.log
            .iter()
            .flat_map(|t| &t.parts)
            .filter_map(|part| match part {
                Part::Stored(n, _) => Some(*n),
                Part::Inline(_) => None,
            })
            .collect()
    }
}

/// A committed transaction in the replayed log.
pub(crate) struct Transaction {
    /// Database size after it.
    pub size: u64,
    /// Pages it changed.
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
    pub(crate) fn append(&mut self, size: u64, images: Images, bytes: usize) {
        self.transactions.push(Transaction {
            size,
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

/// Replay `commit`'s log: its inline segments, and the stored values it names,
/// those not in `known` read together. Returns the log, and every stored value
/// it named.
pub(crate) async fn replay(
    store: &dyn KvStore,
    commit: &Commit,
    mut known: Values,
) -> io::Result<(Log, Values)> {
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
    let mut log = Log::new();
    for transaction in &commit.log {
        let mut images = Images::new();
        let mut bytes = 0;
        for part in &transaction.parts {
            let segment = match part {
                Part::Inline(segment) => segment.clone(),
                Part::Stored(n, index) => segments
                    .get(n)
                    .and_then(|value| value.get(*index as usize))
                    .cloned()
                    .ok_or_else(|| invalid("log segment"))?,
            };
            bytes += segment.len();
            // The parts of a transaction decode against the log before it.
            images.extend(decode_segment(&segment, &log.chain)?);
        }
        log.append(transaction.size, images, bytes);
    }
    Ok((log, known))
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

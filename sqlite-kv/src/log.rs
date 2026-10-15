//! The durable log: commit records, replay and the checkpoint record.
//!
//! A save is one transaction of page images, taken after the private engine
//! checkpoint, so each changed page appears once: the deduplicated committed WAL
//! frames. A transaction occupies consecutive segments `log/<first>…log/<last>`.
//! Each segment records `first`, and only the last carries the commit flag. Parts
//! before the last are written concurrently; the commit part is written after
//! they are acknowledged, and its acknowledgement commits the save.
//!
//! Replay starts at the checkpointed log start and reads segments concurrently in
//! batches. It applies a transaction when it reaches its commit part, and stops at
//! the first absent key or at a segment that does not continue the current
//! transaction. Anything after that point is debris from a failed save: the next
//! save overwrites it from [`Log::next`], and ignoring it is safe because a commit
//! part is only written after its earlier parts were acknowledged.
//!
//! # Compression against the previous images
//!
//! A commit rewrites the same few pages as the commits before it, each a little
//! changed. A segment's images are one zstd frame whose dictionary (a raw
//! prefix) is the previous image of each of its pages: so a page costs about
//! what changed in it. The previous images are those of the [`Chain`]: the
//! latest image of each page in the committed transactions from the chain's
//! base, which the segment records. Replay rebuilds the chain as it goes, so
//! a segment decodes only after the ones before it in its chain.
//!
//! A checkpoint restarts the chain at the log's end once its pages are
//! acknowledged, before it moves `meta`: so every segment's base is at or
//! after any log start `meta` may hold, and replay from that log start has the
//! whole chain. A segment with a later base than the chain so far restarts it.
//!
//! Segment encoding: `SL14`, flags u8 (bit 0: commit), then LEB128 varints:
//! the sequence, how far its transaction's first sequence is before it, how
//! far the chain base is before that, the database size in 512-byte units and
//! the entry count; per entry in ascending page order, the gap from the
//! previous entry's page (the page itself for the first) shifted left once,
//! its low bit the kind (0: a hole, 1: an image); then one zstd frame, with a
//! checksum, of the images in entry order (none when every entry is a hole).
use crate::{
    format::{LOG_MAGIC, META_MAGIC, log_key},
    io::BLOCK_SIZE,
    kv::KvStore,
    page_codec::Page,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    ops::Range,
};
use zstd_safe::{CCtx, CParameter, DCtx};

/// Largest segment value.
pub const MAX_SEGMENT_BYTES: usize = 1024 * 1024;
/// Most entries of a segment: their images and the frame's overhead stay
/// below [`MAX_SEGMENT_BYTES`]. Larger transactions span several segments.
pub const MAX_SEGMENT_PAGES: usize = 240;
const MAGIC: usize = 4;
const ZSTD_LEVEL: i32 = 3;
/// Upper bound on concurrent probe gets.
const MAX_PROBE: u64 = 64;
/// Largest sequence number a stored record may name, far from overflow.
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
/// each page in the committed transactions from `base`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Chain {
    pub base: u64,
    pub images: Images,
}

impl Chain {
    pub(crate) fn new(base: u64) -> Self {
        Self {
            base,
            images: Images::new(),
        }
    }

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

    /// Follow a segment with chain base `base`: a later one restarts the chain.
    fn follow(&mut self, base: u64) -> io::Result<()> {
        if base < self.base {
            return Err(invalid("log segment chain"));
        }
        if base > self.base {
            *self = Self::new(base);
        }
        Ok(())
    }
}

/// A segment's header: what replay reads before decompressing.
#[derive(Debug, PartialEq)]
pub(crate) struct Header {
    pub first: u64,
    pub commit: bool,
    pub size: u64,
    pub base: u64,
    /// Each entry's page, and whether it has an image (or is a hole).
    pub pages: Vec<(u64, bool)>,
}

/// One decoded segment.
#[derive(Debug, PartialEq)]
pub(crate) struct Segment {
    pub first: u64,
    pub commit: bool,
    pub size: u64,
    pub base: u64,
    pub images: Images,
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

/// Split a transaction's images into encoded segments starting at `first`,
/// compressed against `chain`.
pub(crate) fn encode_transaction(
    first: u64,
    size: u64,
    images: &Images,
    chain: &Chain,
) -> io::Result<Vec<Vec<u8>>> {
    let entries: Vec<_> = images
        .iter()
        .map(|(&index, image)| (index, image))
        .collect();
    let parts: Vec<_> = match entries.is_empty() {
        true => vec![&entries[..]],
        false => entries.chunks(MAX_SEGMENT_PAGES).collect(),
    };
    let last = parts.len() - 1;
    (parts.into_iter().enumerate())
        .map(|(n, entries)| {
            encode_segment(first + n as u64, first, n == last, size, entries, chain)
        })
        .collect()
}

/// One segment of the transaction at `first`.
pub(crate) fn encode_segment(
    seq: u64,
    first: u64,
    commit: bool,
    size: u64,
    entries: &[(u64, &Option<Page>)],
    chain: &Chain,
) -> io::Result<Vec<u8>> {
    let zstd = |e: zstd_safe::ErrorCode| io::Error::other(zstd_safe::get_error_name(e));
    let pages: Vec<(u64, bool)> = entries
        .iter()
        .map(|(index, image)| (*index, image.is_some()))
        .collect();
    let mut out = Vec::with_capacity(32 + 2 * entries.len());
    out.extend_from_slice(LOG_MAGIC);
    out.push(u8::from(commit));
    put_varint(&mut out, seq);
    put_varint(&mut out, seq - first);
    put_varint(&mut out, first - chain.base);
    put_varint(&mut out, size / 512);
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
    debug_assert!(out.len() <= MAX_SEGMENT_BYTES);
    Ok(out)
}

/// Decode a segment's header and check that it carries the sequence number
/// of its key; the images follow at the returned offset.
pub(crate) fn header(bytes: &[u8], seq: u64) -> io::Result<(Header, usize)> {
    let segment = || invalid("log segment");
    if bytes.len() <= MAGIC || bytes.len() > MAX_SEGMENT_BYTES || &bytes[..MAGIC] != LOG_MAGIC {
        return Err(segment());
    }
    let flags = bytes[MAGIC];
    let mut at = MAGIC + 1;
    let mut varint = || get_varint(bytes, &mut at).ok_or_else(segment);
    let stored = varint()?;
    let first = stored.checked_sub(varint()?).ok_or_else(segment)?;
    let base = first.checked_sub(varint()?).ok_or_else(segment)?;
    let size = varint()?.checked_mul(512).ok_or_else(segment)?;
    let count = varint()?;
    if stored != seq || flags > 1 || !valid_size(size) || count > MAX_SEGMENT_PAGES as u64 {
        return Err(segment());
    }
    let mut pages: Vec<(u64, bool)> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let entry = varint()?;
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
    let header = Header {
        first,
        commit: flags == 1,
        size,
        base,
        pages,
    };
    Ok((header, at))
}

/// Decode a segment that continues `chain`, checking that it carries the
/// sequence number of its key. A later chain base restarts `chain`.
pub(crate) fn decode(bytes: &Bytes, seq: u64, chain: &mut Chain) -> io::Result<Segment> {
    let (header, at) = header(bytes, seq)?;
    chain.follow(header.base)?;
    let count = header.pages.iter().filter(|(_, image)| *image).count();
    let mut images = Images::new();
    if count > 0 {
        let dictionary = chain.dictionary(header.pages.iter());
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
        for &(index, image) in &header.pages {
            let page = image.then(|| {
                next += BLOCK_SIZE;
                raw.slice(next - BLOCK_SIZE..next)
            });
            images.insert(index, page);
        }
    } else {
        images.extend(header.pages.iter().map(|&(index, _)| (index, None)));
    }
    Ok(Segment {
        first: header.first,
        commit: header.commit,
        size: header.size,
        base: header.base,
        images,
    })
}

/// The checkpoint record at `meta`.
///
/// Encoding: `SM14`, log start u64 LE, deleted-below u64 LE, size u64 LE,
/// database id u64 LE. Segments before `log_start` are checkpointed into pages;
/// those before `deleted_below` are also deleted. `size` is the database size at
/// `log_start`. `id` is random, chosen by the first checkpoint and kept by every
/// later one: a cache can tell this database from another under the same key,
/// such as one written after the namespace was emptied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub log_start: u64,
    pub deleted_below: u64,
    pub size: u64,
    /// Nonzero in a stored record; zero only in the default for an absent one.
    pub id: u64,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            log_start: 1,
            deleted_below: 1,
            size: 0,
            id: 0,
        }
    }
}

impl Meta {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = META_MAGIC.to_vec();
        for n in [self.log_start, self.deleted_below, self.size, self.id] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != 36 || &bytes[..4] != META_MAGIC {
            return Err(invalid("checkpoint record"));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let meta = Self {
            log_start: u64_at(4),
            deleted_below: u64_at(12),
            size: u64_at(20),
            id: u64_at(28),
        };
        if meta.id == 0
            || meta.deleted_below == 0
            || meta.deleted_below > meta.log_start
            || meta.log_start > MAX_SEQUENCE
            || !valid_size(meta.size)
        {
            return Err(invalid("checkpoint record"));
        }
        Ok(meta)
    }
}

/// A random nonzero 64-bit number: a database id, or a starting point for
/// identities that count up.
pub(crate) fn random_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos()),
    );
    hasher.finish().max(1)
}

/// A committed transaction found by replay.
pub(crate) struct Transaction {
    /// One past its commit part.
    pub end: u64,
    /// Database size after it.
    pub size: u64,
    /// Pages it changed.
    pub pages: Vec<u64>,
    /// Encoded bytes of its segments.
    pub bytes: usize,
}

/// Committed transactions replayed from a log start.
pub(crate) struct Log {
    pub transactions: Vec<Transaction>,
    /// Latest committed image of every page the log changes.
    pub images: Images,
    /// Sequence number for the next transaction: one past the last commit.
    pub next: u64,
    /// The last commit part, for confirmation on weak backends.
    pub tail: Option<Bytes>,
    /// Encoded bytes of the committed segments.
    pub bytes: usize,
    /// What the next transaction is compressed against.
    pub chain: Chain,
}

impl Log {
    /// An empty log at `start`.
    pub(crate) fn new(start: u64) -> Self {
        Self {
            transactions: Vec::new(),
            images: Images::new(),
            next: start,
            tail: None,
            bytes: 0,
            chain: Chain::new(start),
        }
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.transactions.last().map(|t| t.size)
    }

    /// Add a transaction this owner committed at [`Self::next`].
    pub(crate) fn append(
        &mut self,
        end: u64,
        size: u64,
        images: Images,
        bytes: usize,
        tail: Bytes,
    ) {
        self.transactions.push(Transaction {
            end,
            size,
            pages: images.keys().copied().collect(),
            bytes,
        });
        self.chain.images.extend(images.clone());
        self.images.extend(images);
        self.next = end;
        self.bytes += bytes;
        self.tail = Some(tail);
    }

    /// Drop the transactions before `start`, a transaction boundary that a
    /// checkpoint has covered. Pages only they changed leave [`Self::images`].
    pub(crate) fn drop_before(&mut self, start: u64) {
        let covered = self
            .transactions
            .iter()
            .take_while(|t| t.end <= start)
            .count();
        if covered == 0 {
            return;
        }
        let dropped: Vec<_> = self.transactions.drain(..covered).collect();
        let kept: std::collections::HashSet<u64> = self
            .transactions
            .iter()
            .flat_map(|t| t.pages.iter().copied())
            .collect();
        for transaction in dropped {
            self.bytes -= transaction.bytes;
            for page in transaction.pages {
                if !kept.contains(&page) {
                    self.images.remove(&page);
                }
            }
        }
    }
}

/// Probe batch size for an anticipated number of segments.
pub(crate) fn batch(expected: u64) -> u64 {
    (expected + 2).clamp(2, MAX_PROBE)
}

/// Read `batch` consecutive segment keys from `first` concurrently.
pub(crate) async fn probe(
    store: &dyn KvStore,
    first: u64,
    batch: u64,
) -> io::Result<Vec<Option<Bytes>>> {
    let end = first
        .checked_add(batch)
        .filter(|&end| end <= MAX_SEQUENCE)
        .ok_or_else(|| invalid("log position"))?;
    let keys: Vec<_> = (first..end).map(log_key).collect();
    try_join_all(keys.iter().map(|key| store.get(key))).await
}

/// The pages changed by the committed transactions in `range`, which must
/// start and end at transaction boundaries, from the segments' headers. `None`
/// when that cannot be read in one probe batch: garbage collection deleted
/// some of it, or it is longer than a batch. `prefetched` is an already read
/// probe at `range.start`.
pub(crate) async fn changes(
    store: &dyn KvStore,
    range: Range<u64>,
    prefetched: Option<Vec<Option<Bytes>>>,
) -> io::Result<Option<BTreeSet<u64>>> {
    let len = range.end - range.start;
    if len > MAX_PROBE {
        return Ok(None);
    }
    let values = match prefetched {
        Some(values) if values.len() as u64 >= len => values,
        _ if len == 0 => Vec::new(),
        _ => probe(store, range.start, len).await?,
    };
    let mut pages = BTreeSet::new();
    let mut first = range.start;
    for (seq, value) in range.clone().zip(values) {
        let Some((header, _)) = value.and_then(|bytes| header(&bytes, seq).ok()) else {
            return Ok(None);
        };
        if header.first != first {
            return Ok(None);
        }
        if header.commit {
            first = seq + 1;
        }
        pages.extend(header.pages.iter().map(|&(index, _)| index));
    }
    Ok((first == range.end).then_some(pages))
}

/// Replay committed transactions from `start`, probing `batch` keys first and
/// twice as many each round after, up to [`MAX_PROBE`]: a short log costs few
/// reads past its end, a long one few rounds. `prefetched` is an already read
/// first probe batch at `start`.
pub(crate) async fn replay(
    store: &dyn KvStore,
    start: u64,
    mut batch: u64,
    prefetched: Option<Vec<Option<Bytes>>>,
) -> io::Result<Log> {
    let mut log = Log::new(start);
    let mut pending_bytes = 0;
    let mut seq = start;
    let mut pending: Vec<Images> = Vec::new();
    let mut next = prefetched;
    'probe: loop {
        let values = match next.take() {
            Some(values) => values,
            None => probe(store, seq, batch).await?,
        };
        batch = (batch * 2).min(MAX_PROBE);
        for value in values {
            let Some(bytes) = value else { break 'probe };
            if header(&bytes, seq)?.0.first != log.next {
                break 'probe; // debris of a failed save
            }
            // The parts of a transaction decode against the chain before it.
            let segment = decode(&bytes, seq, &mut log.chain)?;
            seq += 1;
            pending_bytes += bytes.len();
            pending.push(segment.images);
            if segment.commit {
                let mut pages = Vec::new();
                for images in pending.drain(..) {
                    pages.extend(images.keys());
                    log.chain.images.extend(images.clone());
                    log.images.extend(images);
                }
                let committed = std::mem::take(&mut pending_bytes);
                log.transactions.push(Transaction {
                    end: seq,
                    size: segment.size,
                    pages,
                    bytes: committed,
                });
                log.next = seq;
                log.bytes += committed;
                log.tail = Some(bytes);
            }
        }
    }
    Ok(log)
}

#[cfg(test)]
#[path = "../tests/unit/log.rs"]
mod tests;

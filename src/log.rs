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
//! Segment encoding: `SL12`, sequence u64 LE, first sequence of its transaction
//! u64 LE, flags u8 (bit 0: commit), database size u64 LE, entry count u32 LE, then
//! per entry in ascending page order: page index u64 LE, encoded length u16 LE
//! (zero for a hole) and the [`crate::page_codec`] value.
use crate::{
    format::{LOG_MAGIC, META_MAGIC, log_key},
    kv::KvStore,
    page_codec::Page,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{collections::BTreeMap, io, ops::Range};

/// Largest segment value; larger transactions span several segments.
pub const MAX_SEGMENT_BYTES: usize = 1024 * 1024;
const PREFIX: usize = 33;
const ENTRY_HEADER: usize = 10;
/// Upper bound on concurrent probe gets.
const MAX_PROBE: u64 = 64;

/// Encoded page images by page number; `None` is a hole.
pub(crate) type Images = BTreeMap<u64, Option<Page>>;

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid {what}"))
}

/// One decoded segment.
#[derive(Debug, PartialEq)]
pub(crate) struct Segment {
    pub first: u64,
    pub commit: bool,
    pub size: u64,
    pub images: Images,
}

/// Split a transaction's images into encoded segments starting at `first`.
pub(crate) fn encode_transaction(first: u64, size: u64, images: &Images) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<(u64, &[u8])>> = vec![Vec::new()];
    let mut bytes = PREFIX;
    for (&index, image) in images {
        let image = image.as_deref().unwrap_or_default();
        let entry = ENTRY_HEADER + image.len();
        if bytes + entry > MAX_SEGMENT_BYTES && !parts.last().unwrap().is_empty() {
            parts.push(Vec::new());
            bytes = PREFIX;
        }
        parts.last_mut().unwrap().push((index, image));
        bytes += entry;
    }
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(n, entries)| {
            let mut out = Vec::with_capacity(PREFIX + entries.len() * 4096);
            out.extend_from_slice(LOG_MAGIC);
            out.extend_from_slice(&(first + n as u64).to_le_bytes());
            out.extend_from_slice(&first.to_le_bytes());
            out.push(u8::from(n == last));
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for (index, image) in entries {
                out.extend_from_slice(&index.to_le_bytes());
                out.extend_from_slice(&(image.len() as u16).to_le_bytes());
                out.extend_from_slice(image);
            }
            out
        })
        .collect()
}

/// Decode a segment and check that it carries the sequence number of its key.
/// Page images are validated by decoding them.
/// Page images share the segment's buffer.
pub(crate) fn decode(bytes: &Bytes, seq: u64) -> io::Result<Segment> {
    let segment = || invalid("log segment");
    if bytes.len() < PREFIX || bytes.len() > MAX_SEGMENT_BYTES || &bytes[..4] != LOG_MAGIC {
        return Err(segment());
    }
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let first = u64_at(12);
    if u64_at(4) != seq || first > seq || bytes[20] > 1 {
        return Err(segment());
    }
    let count = u32::from_le_bytes(bytes[29..33].try_into().unwrap());
    let mut at = PREFIX;
    let mut images = Images::new();
    for _ in 0..count {
        let header = bytes.get(at..at + ENTRY_HEADER).ok_or_else(segment)?;
        let index = u64::from_le_bytes(header[..8].try_into().unwrap());
        let len = u16::from_le_bytes([header[8], header[9]]) as usize;
        at += ENTRY_HEADER;
        if at + len > bytes.len()
            || images
                .last_key_value()
                .is_some_and(|(&last, _)| last >= index)
        {
            return Err(segment());
        }
        let image = bytes.slice(at..at + len);
        if len != 0 {
            crate::page_codec::decode_bytes(&image)?;
        }
        images.insert(index, (len != 0).then_some(image));
        at += len;
    }
    if at != bytes.len() {
        return Err(segment());
    }
    Ok(Segment {
        first,
        commit: bytes[20] == 1,
        size: u64_at(21),
        images,
    })
}

/// The checkpoint record at `meta`.
///
/// Encoding: `SM12`, log start u64 LE, deleted-below u64 LE, size u64 LE. Segments
/// before `log_start` are checkpointed into pages; those before `deleted_below`
/// are also deleted. `size` is the database size at `log_start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub log_start: u64,
    pub deleted_below: u64,
    pub size: u64,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            log_start: 1,
            deleted_below: 1,
            size: 0,
        }
    }
}

impl Meta {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = META_MAGIC.to_vec();
        for n in [self.log_start, self.deleted_below, self.size] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != 28 || &bytes[..4] != META_MAGIC {
            return Err(invalid("checkpoint record"));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let meta = Self {
            log_start: u64_at(4),
            deleted_below: u64_at(12),
            size: u64_at(20),
        };
        if meta.deleted_below == 0 || meta.deleted_below > meta.log_start {
            return Err(invalid("checkpoint record"));
        }
        Ok(meta)
    }
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
}

impl Log {
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
    let keys: Vec<_> = (first..first + batch).map(log_key).collect();
    try_join_all(keys.iter().map(|key| store.get(key))).await
}

/// Latest images of the pages changed by the committed transactions in `range`,
/// which must start and end at transaction boundaries. `None` when that cannot be
/// read in one probe batch: garbage collection deleted some of it, or it is
/// longer than a batch. `prefetched` is an already read probe at `range.start`.
pub(crate) async fn changes(
    store: &dyn KvStore,
    range: Range<u64>,
    prefetched: Option<Vec<Option<Bytes>>>,
) -> io::Result<Option<Images>> {
    let len = range.end - range.start;
    if len > MAX_PROBE {
        return Ok(None);
    }
    let values = match prefetched {
        Some(values) if values.len() as u64 >= len => values,
        _ if len == 0 => Vec::new(),
        _ => probe(store, range.start, len).await?,
    };
    let mut images = Images::new();
    let mut first = range.start;
    for (seq, value) in range.clone().zip(values) {
        let Some(segment) = value.and_then(|bytes| decode(&bytes, seq).ok()) else {
            return Ok(None);
        };
        if segment.first != first {
            return Ok(None);
        }
        if segment.commit {
            first = seq + 1;
        }
        images.extend(segment.images);
    }
    Ok((first == range.end).then_some(images))
}

/// Replay committed transactions from `start`. `prefetched` is an already read
/// first probe batch at `start`.
pub(crate) async fn replay(
    store: &dyn KvStore,
    start: u64,
    batch: u64,
    prefetched: Option<Vec<Option<Bytes>>>,
) -> io::Result<Log> {
    let mut log = Log {
        transactions: Vec::new(),
        images: Images::new(),
        next: start,
        tail: None,
        bytes: 0,
    };
    let mut pending_bytes = 0;
    let mut seq = start;
    let mut pending: Vec<Images> = Vec::new();
    let mut next = prefetched;
    'probe: loop {
        let values = match next.take() {
            Some(values) => values,
            None => probe(store, seq, batch).await?,
        };
        for value in values {
            let Some(bytes) = value else { break 'probe };
            let segment = decode(&bytes, seq)?;
            if segment.first != log.next {
                break 'probe; // debris of a failed save
            }
            seq += 1;
            pending_bytes += bytes.len();
            pending.push(segment.images);
            if segment.commit {
                let mut pages = Vec::new();
                for images in pending.drain(..) {
                    pages.extend(images.keys());
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

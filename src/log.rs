//! Durable delta log: a save can commit with one put instead of a full publication.
//!
//! When [`crate::StorageOptions::max_log_segments`] is nonzero and the head has the
//! log enabled (`log_start != 0`), a save writes the pages it changed as one
//! segment at the next sequence number `log/<start + count>`. That single atomic,
//! durable put is the commit point; no intent, map node or head is written.
//! Segments are page images after the private engine checkpoint, so each changed
//! page appears once per save, which is equivalent to shipping the deduplicated
//! committed WAL frames.
//!
//! Open loads the head, then reads `log/<start>`, `log/<start + 1>`, … concurrently
//! in batches until the first absent key, and replays the segments over the
//! snapshot in sequence order. A key present after an absent one is rejected.
//! With an [`crate::ObjectCache`], the first batch is read speculatively together
//! with the head at the last `log_start` the cache saw; it is discarded if the
//! head disagrees.
//! Before exposing any page, open re-puts the last segment: an append that failed
//! may be visible without being durable, and a later segment must never build on
//! one that a crash could still remove. Every earlier segment was confirmed this
//! way, or acknowledged, before its successor was written.
//!
//! When the log holds `max_log_segments` segments, or a segment would exceed
//! [`MAX_SEGMENT_BYTES`], the save folds instead: it publishes log and session
//! pages through the ordinary intent/objects/head protocol and retires the
//! segments in the same intent. A session with the log disabled folds any existing
//! segments and writes `log_start = 0`, so the head says whether to probe at all.
//!
//! Segment encoding: `SL11`, sequence u64 LE, database size u64 LE, entry count
//! u32 LE, then per entry in ascending page order: page index u64 LE, encoded length
//! u16 LE (zero for a hole) and the [`crate::page_codec`] value.
use crate::{
    format::Kind,
    kv::KvStore,
    page_codec::{self, PageCompression},
};
use futures::future::try_join_all;
use std::{collections::BTreeMap, io};

/// Largest segment value; a larger save folds instead of appending.
pub const MAX_SEGMENT_BYTES: usize = 1024 * 1024;
const MAGIC: &[u8; 4] = b"SL11";
const PREFIX: usize = 24;
/// Upper bound on concurrent probe gets.
const MAX_PROBE: usize = 16;

/// Logical page replacements; `None` is a hole.
pub(crate) type Pages = BTreeMap<u64, Option<Vec<u8>>>;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid log segment")
}

pub(crate) fn encode(
    seq: u64,
    size: u64,
    pages: &Pages,
    compression: PageCompression,
) -> io::Result<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for (index, page) in pages {
        let encoded = match page {
            Some(page) => page_codec::encode(page, compression)?,
            None => Vec::new(),
        };
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
        out.extend_from_slice(&encoded);
    }
    Ok(out)
}

/// Decode a segment and check that it carries the sequence number of its key.
pub(crate) fn decode(bytes: &[u8], seq: u64) -> io::Result<(u64, Pages)> {
    if bytes.len() < PREFIX || bytes.len() > MAX_SEGMENT_BYTES || &bytes[..4] != MAGIC {
        return Err(invalid());
    }
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    if u64_at(4) != seq {
        return Err(invalid());
    }
    let size = u64_at(12);
    let count = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    let mut input = &bytes[PREFIX..];
    let mut pages = Pages::new();
    for _ in 0..count {
        let (header, rest) = input.split_at_checked(10).ok_or_else(invalid)?;
        let index = u64::from_le_bytes(header[..8].try_into().unwrap());
        let len = u16::from_le_bytes([header[8], header[9]]) as usize;
        let (encoded, rest) = rest.split_at_checked(len).ok_or_else(invalid)?;
        if pages
            .last_key_value()
            .is_some_and(|(&last, _)| last >= index)
        {
            return Err(invalid());
        }
        let page = (len != 0)
            .then(|| page_codec::decode(encoded))
            .transpose()?;
        pages.insert(index, page);
        input = rest;
    }
    if !input.is_empty() {
        return Err(invalid());
    }
    Ok((size, pages))
}

/// Segments replayed over a snapshot.
#[derive(Default)]
pub(crate) struct Log {
    /// Number of segments found from the head's `log_start`.
    pub count: u64,
    /// Latest page images; they override the snapshot map.
    pub pages: Pages,
    /// Database size recorded by the last segment.
    pub size: Option<u64>,
}

/// Probe batch size for an anticipated segment count; one batch covers it.
pub(crate) fn batch(expected: usize) -> u64 {
    (expected + 1).clamp(1, MAX_PROBE) as u64
}

/// Read `batch` consecutive segment keys from `first` concurrently.
pub(crate) async fn probe(
    store: &dyn KvStore,
    first: u64,
    batch: u64,
) -> io::Result<Vec<Option<Vec<u8>>>> {
    let keys: Vec<_> = (first..first + batch)
        .map(|seq| Kind::Log.key(seq))
        .collect();
    try_join_all(keys.iter().map(|key| store.get(key))).await
}

/// Read and replay the log from `start`, then confirm its last segment durably.
/// `prefetched` is an already completed first probe batch at `start`.
pub(crate) async fn load(
    store: &dyn KvStore,
    start: u64,
    expected: usize,
    prefetched: Option<Vec<Option<Vec<u8>>>>,
) -> io::Result<Log> {
    let mut log = Log::default();
    if start == 0 {
        return Ok(log);
    }
    let mut next = prefetched;
    let mut last = None;
    loop {
        let values = match next.take() {
            Some(values) => values,
            None => probe(store, start + log.count, batch(expected)).await?,
        };
        let found = values.iter().take_while(|v| v.is_some()).count();
        if values[found..].iter().any(Option::is_some) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gap in delta log",
            ));
        }
        let complete = found == values.len();
        for bytes in values.into_iter().flatten() {
            let (size, pages) = decode(&bytes, start + log.count)?;
            log.pages.extend(pages);
            log.size = Some(size);
            log.count += 1;
            last = Some(bytes);
        }
        if !complete {
            break;
        }
    }
    if let Some(bytes) = last {
        store
            .put(&Kind::Log.key(start + log.count - 1), bytes)
            .await?;
    }
    Ok(log)
}

#[cfg(test)]
#[path = "../tests/unit/log.rs"]
mod tests;

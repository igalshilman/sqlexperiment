//! The working-set hint: which pages a cold open should fetch.
//!
//! An owner records the pages its sessions read. Checkpointing saves write the
//! most useful ones to `v12/hint`; a cold open reads that key together with
//! `meta` and fetches the listed pages alongside replay, so its first queries
//! find them without a round trip per B-tree level.
//!
//! The hint is advisory. It is never confirmed or ordered against other writes:
//! a stale, missing or malformed hint only wastes or saves gets, because pages
//! are still read through the normal rules. (A backend error reading it fails
//! open like any other read.)
//!
//! Encoding: `SH12`, entry count u16 LE, then the page numbers in ascending
//! order as LEB128 deltas (the first from zero).
use crate::format::HINT_MAGIC;
use std::collections::HashMap;

/// Most pages a hint may list.
pub const MAX_HINT_PAGES: usize = 256;
/// Pages tracked per owner; the least recently read leaves are dropped first.
const MAX_TRACKED: usize = 4 * MAX_HINT_PAGES;

/// Encode ascending page numbers.
pub(crate) fn encode(pages: &[u64]) -> Vec<u8> {
    let mut out = HINT_MAGIC.to_vec();
    out.extend_from_slice(&(pages.len() as u16).to_le_bytes());
    let mut previous = 0;
    for &page in pages {
        let mut delta = page - previous;
        previous = page;
        loop {
            let byte = (delta & 0x7f) as u8;
            delta >>= 7;
            if delta == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    out
}

/// Decode a hint; `None` if it is malformed or longer than `limit` pages.
pub(crate) fn decode(bytes: &[u8], limit: usize) -> Option<Vec<u64>> {
    let rest = bytes.strip_prefix(HINT_MAGIC)?;
    let count = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
    if count > limit.min(MAX_HINT_PAGES) {
        return None;
    }
    let mut pages = Vec::with_capacity(count);
    let mut bytes = rest[2..].iter();
    let mut page = 0u64;
    for _ in 0..count {
        let (mut delta, mut shift) = (0u64, 0);
        loop {
            let byte = *bytes.next()?;
            delta |= u64::from(byte & 0x7f).checked_shl(shift)?;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        page = page.checked_add(delta)?;
        pages.push(page);
    }
    bytes.next().is_none().then_some(pages)
}

/// Pages read recently, and whether each looked like an interior B-tree page.
#[derive(Default)]
pub(crate) struct Usage {
    clock: u64,
    pages: HashMap<u64, (u64, bool)>,
}

impl Usage {
    /// Record a read of `page` with the first bytes of its content.
    pub(crate) fn record(&mut self, page: u64, content: &[u8]) {
        // B-tree page type: 0x02 interior index, 0x05 interior table. Page 0
        // (the file header and schema root) is kept like an interior page.
        let interior = page == 0 || matches!(content.first(), Some(0x02 | 0x05));
        self.clock += 1;
        self.pages.insert(page, (self.clock, interior));
        if self.pages.len() > MAX_TRACKED {
            let mut leaves: Vec<_> = self
                .pages
                .iter()
                .filter(|(_, (_, interior))| !interior)
                .map(|(&page, &(used, _))| (used, page))
                .collect();
            leaves.sort_unstable();
            for (_, page) in leaves.iter().take(self.pages.len() - MAX_TRACKED / 2) {
                self.pages.remove(page);
            }
        }
    }

    /// The `limit` most useful pages, ascending: page 0, then interior pages,
    /// then leaves, each most recently read first.
    pub(crate) fn top(&self, limit: usize) -> Vec<u64> {
        let mut ranked: Vec<_> = self
            .pages
            .iter()
            .map(|(&page, &(used, interior))| ((page != 0, !interior, u64::MAX - used), page))
            .collect();
        ranked.sort_unstable();
        let mut pages: Vec<u64> = ranked
            .into_iter()
            .take(limit.min(MAX_HINT_PAGES))
            .map(|(_, page)| page)
            .collect();
        pages.sort_unstable();
        pages
    }
}

#[cfg(test)]
#[path = "../tests/unit/hint.rs"]
mod tests;

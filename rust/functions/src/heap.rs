//! The guest's heap, as both engines' allocator (wasi-libc's dlmalloc,
//! 32-bit) lays it out: chunks back to back from the first to the end of
//! memory, each a 4-byte footer slot and a 4-byte head (its size, with bit 1
//! set when in use and bit 0 when the chunk before it is). The last is the
//! allocator's top foot.
//!
//! A free chunk's bytes past its links are dead: nothing reads them before an
//! allocation writes them (`calloc` clears its memory). Capture zeroes them in
//! the pages it compares, so what a turn allocated and freed is not saved.
use std::ops::Range;

/// QuickJS's first chunk: the start of the heap, after the pinned binary's
/// static data. Golden capture checks that the heap walks from here.
pub(crate) const QUICKJS: usize = 0x11_d048;

/// The first chunk of a heap that starts at `heap_base`: dlmalloc aligns the
/// memory it hands out, 8 bytes past a chunk's start, to 16 bytes.
pub(crate) fn first_chunk(heap_base: usize) -> usize {
    (heap_base + 8).next_multiple_of(16) - 8
}
/// A free chunk's footer slot, head, and links: its list's, or its tree's.
const LINKS: usize = 32;

/// The dead bytes of `memory`'s heap, whose first chunk is at `start`, in
/// order; `None` if the heap does not walk as dlmalloc's, and then none are
/// known to be dead.
pub(crate) fn dead(memory: &[u8], start: usize) -> Option<Vec<Range<usize>>> {
    walk(memory, start).map(|(_, dead)| dead)
}

/// The heap's chunks as a walk found them: where each starts, in order, the
/// top foot last.
///
/// A later walk takes them as they were wherever no page was written since:
/// a chunk whose head is on such a page has its size still, and so the chunk
/// after it starts where it did. It reads the heap only around the pages
/// written, from the last chunk that starts before them to the first one
/// after them whose head was not written.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chunks {
    starts: Vec<u32>,
    /// The length of the memory walked.
    len: usize,
}

/// One chunk: its size, whether it is in use, and where the next starts
/// (`None` past the top foot, which ends memory).
struct Chunk {
    size: usize,
    in_use: bool,
    previous_in_use: bool,
    next: Option<usize>,
}

fn chunk(memory: &[u8], at: usize) -> Option<Chunk> {
    let head = u32::from_le_bytes(memory.get(at + 4..at + 8)?.try_into().ok()?) as usize;
    let size = head & !7;
    if size < 16 || !size.is_multiple_of(8) {
        return None;
    }
    let next = at.checked_add(size).filter(|&next| next <= memory.len())?;
    Some(Chunk {
        size,
        in_use: head & 2 != 0,
        previous_in_use: head & 1 != 0,
        next: (next < memory.len()).then_some(next),
    })
}

/// Walk `memory`'s whole heap from its first chunk, at `start`: its chunks,
/// and the dead bytes of the free ones.
pub(crate) fn walk(memory: &[u8], start: usize) -> Option<(Chunks, Vec<Range<usize>>)> {
    let mut chunks = Chunks {
        starts: Vec::new(),
        len: memory.len(),
    };
    let mut dead = Vec::new();
    let (mut at, mut previous_in_use) = (start, true);
    loop {
        let found = chunk(memory, at)?;
        if found.previous_in_use != previous_in_use {
            return None;
        }
        chunks.starts.push(at as u32);
        let Some(next) = found.next else {
            // The top foot, after the free top chunk.
            return (!found.in_use && !previous_in_use).then_some((chunks, dead));
        };
        if !found.in_use {
            // Free chunks never touch: freeing one merges it with its neighbors.
            if !previous_in_use {
                return None;
            }
            if found.size > LINKS {
                dead.push(at + LINKS..next);
            }
        }
        (at, previous_in_use) = (next, found.in_use);
    }
}

impl Chunks {
    /// The chunks after a turn wrote the pages `written` (in order), and the
    /// dead bytes of the free chunks that reach into them; `None` where the
    /// heap does not walk as before, and then a whole walk must.
    pub(crate) fn update(
        &self,
        memory: &[u8],
        written: &[usize],
    ) -> Option<(Chunks, Vec<Range<usize>>)> {
        use crate::model::PAGE;
        if memory.len() != self.len || self.starts.is_empty() {
            return None;
        }
        let written_at = |at: usize| written.binary_search(&(at / PAGE)).is_ok();
        let old = &self.starts;
        let mut starts = Vec::with_capacity(old.len());
        let mut dead = Vec::new();
        // The old chunks before `kept` are taken as they are; `cursor` is
        // where the walk stands, a chunk start.
        let (mut kept, mut cursor) = (0, old[0] as usize);
        let mut run = 0;
        while run < written.len() {
            let first = written[run];
            while run + 1 < written.len() && written[run + 1] == written[run] + 1 {
                run += 1;
            }
            let end = (written[run] + 1) * PAGE;
            run += 1;
            if cursor >= end {
                continue;
            }
            // Resume at the last old chunk that starts before the written pages:
            // its head, on a page not written, still holds its size.
            let before = kept + old[kept..].partition_point(|&at| (at as usize) < first * PAGE);
            if before > kept {
                starts.extend_from_slice(&old[kept..before - 1]);
                cursor = old[before - 1] as usize;
            }
            let mut previous_in_use = None;
            loop {
                let found = chunk(memory, cursor)?;
                if previous_in_use.is_some_and(|previous| previous != found.previous_in_use) {
                    return None;
                }
                starts.push(cursor as u32);
                let Some(next) = found.next else {
                    // The top foot: memory ends here.
                    if found.in_use || found.previous_in_use {
                        return None;
                    }
                    return Some((
                        Chunks {
                            starts,
                            len: self.len,
                        },
                        dead,
                    ));
                };
                if !found.in_use {
                    if previous_in_use == Some(false) {
                        return None;
                    }
                    if found.size > LINKS {
                        dead.push(cursor + LINKS..next);
                    }
                }
                previous_in_use = Some(found.in_use);
                cursor = next;
                if cursor >= end && !written_at(cursor) {
                    break;
                }
            }
            // A chunk whose head no turn wrote started a chunk before too.
            kept = old.binary_search(&(cursor as u32)).ok()?;
        }
        starts.extend_from_slice(&old[kept..]);
        Some((
            Chunks {
                starts,
                len: self.len,
            },
            dead,
        ))
    }
}

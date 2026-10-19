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
    let word = |at: usize| -> Option<usize> {
        Some(u32::from_le_bytes(memory.get(at..at + 4)?.try_into().ok()?) as usize)
    };
    let mut dead = Vec::new();
    let (mut at, mut previous_in_use) = (start, true);
    loop {
        let head = word(at + 4)?;
        let (size, in_use) = (head & !7, head & 2 != 0);
        if size < 16 || !size.is_multiple_of(8) || (head & 1 != 0) != previous_in_use {
            return None;
        }
        let next = at.checked_add(size).filter(|&next| next <= memory.len())?;
        if next == memory.len() {
            // The top foot, after the free top chunk.
            return (!in_use && !previous_in_use).then_some(dead);
        }
        if !in_use {
            // Free chunks never touch: freeing one merges it with its neighbors.
            if !previous_in_use {
                return None;
            }
            if size > LINKS {
                dead.push(at + LINKS..next);
            }
        }
        (at, previous_in_use) = (next, in_use);
    }
}

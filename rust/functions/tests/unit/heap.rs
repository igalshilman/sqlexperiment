use crate::heap::*;

const IN_USE: u32 = 2;
const PREVIOUS_IN_USE: u32 = 1;

/// A heap of `chunks` (size, in use), then the top foot, ending memory.
fn heap(chunks: &[(usize, bool)]) -> Vec<u8> {
    let mut memory = vec![0xAB; QUICKJS];
    let mut previous_in_use = true;
    for &(size, in_use) in chunks.iter().chain([(48, false)].iter()) {
        let at = memory.len();
        memory.resize(at + size, 0xAB);
        let flags = u32::from(in_use) * IN_USE + u32::from(previous_in_use) * PREVIOUS_IN_USE;
        memory[at + 4..at + 8].copy_from_slice(&(size as u32 | flags).to_le_bytes());
        previous_in_use = in_use;
    }
    memory
}

#[test]
fn free_chunks_are_dead_past_their_links() {
    let memory = heap(&[
        (64, true),
        (128, false),
        (32, true),
        (24, false),
        (16, true),
        (256, false),
    ]);
    let free = QUICKJS + 64;
    let top = QUICKJS + 64 + 128 + 32 + 24 + 16;
    assert_eq!(
        dead(&memory, QUICKJS),
        Some(vec![free + 32..free + 128, top + 32..top + 256])
    );
}

#[test]
fn a_heap_that_does_not_walk_has_nothing_dead() {
    // Two free chunks side by side.
    assert_eq!(
        dead(
            &heap(&[
                (64, true),
                (64, false),
                (64, false),
                (64, true),
                (64, false)
            ]),
            QUICKJS
        ),
        None
    );
    // A chunk past the end of memory.
    let mut memory = heap(&[(64, true), (64, false)]);
    memory.truncate(memory.len() - 8);
    assert_eq!(dead(&memory, QUICKJS), None);
    // A chunk whose flag disagrees with the one before it.
    let mut memory = heap(&[(64, true), (64, false)]);
    memory[QUICKJS + 4] &= !(PREVIOUS_IN_USE as u8);
    assert_eq!(dead(&memory, QUICKJS), None);
    // No heap at all.
    assert_eq!(dead(&vec![0; QUICKJS + 64], QUICKJS), None);
}

/// Pages of `before` and `after` that differ.
fn written(before: &[u8], after: &[u8]) -> Vec<usize> {
    let page = crate::model::PAGE;
    (0..after.len() / page)
        .filter(|index| {
            before[index * page..(index + 1) * page] != after[index * page..(index + 1) * page]
        })
        .collect()
}

/// A heap of `chunks`, padded at the front so it ends on a page.
fn paged(chunks: &[(usize, bool)]) -> (Vec<u8>, usize) {
    let page = crate::model::PAGE;
    let mut layout = chunks.to_vec();
    let total: usize = QUICKJS + chunks.iter().map(|(size, _)| size).sum::<usize>() + 48;
    // Grow the first chunk so the heap ends on a page boundary.
    layout[0].0 += total.next_multiple_of(page) - total;
    (heap(&layout), QUICKJS)
}

/// After a turn rewrites some chunks, the partial walk finds the chunks and
/// dead bytes a whole walk does, near the pages written.
#[test]
fn a_partial_walk_finds_what_a_whole_one_does() {
    let page = crate::model::PAGE;
    let big = 3 * page;
    let mut layout = vec![(64, true)];
    for i in 0..64 {
        layout.push((if i % 7 == 3 { big } else { 96 }, i % 2 == 0));
    }
    // No two free chunks side by side: every other is in use.
    let (before, start) = paged(&layout);
    let (chunks, _) = walk(&before, start).expect("the heap walks");
    let check = |after: Vec<u8>| {
        let pages = written(&before, &after);
        let (partial, dead) = chunks.update(&after, &pages).expect("partial walk");
        let (whole, everywhere) = walk(&after, start).expect("whole walk");
        assert_eq!(partial, whole);
        let near = |range: &&std::ops::Range<usize>| {
            (range.start / page..range.end.div_ceil(page))
                .any(|index| pages.binary_search(&index).is_ok())
        };
        assert_eq!(
            dead.iter().filter(near).collect::<Vec<_>>(),
            everywhere.iter().filter(near).collect::<Vec<_>>()
        );
    };
    // Allocate a free chunk: its head gains the in-use bit, and so does the
    // next chunk's previous-in-use bit.
    let mut after = before.clone();
    let positions: Vec<usize> = chunks_of(&before, start);
    let free = positions
        .iter()
        .copied()
        .find(|&at| head(&before, at) & 2 == 0 && at > QUICKJS + page)
        .unwrap();
    set_head(&mut after, free, head(&before, free) | 2);
    let next = free + (head(&before, free) & !7) as usize;
    set_head(&mut after, next, head(&before, next) | 1);
    check(after);
    // Free an in-use chunk between two free ones: the three merge into one,
    // and the heads of the second and third stay behind, stale, possibly on
    // pages no one wrote.
    let mut after = before.clone();
    // An in-use chunk between two free ones, well inside the heap.
    let index = (1..positions.len() - 2)
        .find(|&i| {
            head(&before, positions[i]) & 2 != 0
                && head(&before, positions[i - 1]) & 2 == 0
                && positions[i] > QUICKJS + 2 * page
        })
        .unwrap();
    let used = positions[index];
    let _ = used;
    let (previous, next) = (positions[index - 1], positions[index + 1]);
    let after_next = positions[index + 2];
    let merged = after_next - previous;
    set_head(
        &mut after,
        previous,
        merged as u32 | (head(&before, previous) & 1),
    );
    set_head(&mut after, after_next, head(&before, after_next) & !1);
    let _ = next;
    check(after);
}

fn head(memory: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(memory[at + 4..at + 8].try_into().unwrap())
}

fn set_head(memory: &mut [u8], at: usize, head: u32) {
    memory[at + 4..at + 8].copy_from_slice(&head.to_le_bytes());
}

fn chunks_of(memory: &[u8], start: usize) -> Vec<usize> {
    let mut at = start;
    let mut positions = Vec::new();
    while at < memory.len() {
        positions.push(at);
        at += (head(memory, at) & !7) as usize;
    }
    positions
}

/// Memory that grew is walked whole: the partial walk declines.
#[test]
fn a_partial_walk_declines_memory_that_grew() {
    let (before, start) = paged(&[(64, true), (96, false), (64, true), (128, false)]);
    let (chunks, _) = walk(&before, start).unwrap();
    let mut grown = before.clone();
    grown.resize(before.len() + crate::model::PAGE, 0);
    assert!(chunks.update(&grown, &[]).is_none());
}

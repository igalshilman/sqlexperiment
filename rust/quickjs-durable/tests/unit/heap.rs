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

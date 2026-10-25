//! What a cold read of a checkpoint's frames fetches from the store.
#![cfg(feature = "simulation")]
use state::{Result, State, simulation::*};
use std::sync::Arc;

/// A page that compresses to about its size: a few frames share a part.
fn noise(seed: u64) -> state::Page {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..4096)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect::<Vec<u8>>()
        .into()
}

/// With packs kept in Restate, a cold read fetches the 64 KiB part its frame
/// is in, and the cache keeps the part: reading every page of a file costs
/// a get per part of the pack, not one per frame.
#[test]
fn a_part_of_a_pack_is_fetched_once_for_every_frame_in_it() -> Result<()> {
    futures::executor::block_on(async {
        const PAGES: u64 = 64;
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let file = state.file("db")?;
        let mut mutation = state.mutation();
        for n in 0..PAGES {
            mutation.write(&file, n, Some(noise(n)))?;
        }
        mutation.flush().await?;
        // Checkpoint the tail into a pack, with a flush that writes a record.
        let mut mutation = state.mutation();
        mutation.checkpoint();
        mutation.set(&state.record("r"), "x".into())?;
        let report = mutation.flush().await?;
        let pack_bytes = report.checkpoint.expect("a checkpoint").pack_bytes;
        let parts = pack_bytes.div_ceil(64 << 10) as u64;
        let frames = PAGES / 4;
        assert!(parts < frames, "{parts} parts for {frames} frames");

        // A cold cache: another process's.
        let cache = open_cache(cache_config(16 << 20, 16 << 20));
        let reader = State::open(store.clone()).cached(&cache, "db").await?;
        store.reset_stats();
        let file = reader.file("db")?;
        for n in 0..PAGES {
            assert_eq!(file.read(n).await?, Some(noise(n)), "page {n}");
        }
        let gets = store.stats().kind("pack").gets;
        assert_eq!(gets, parts, "a get per part, {frames} frames");
        Ok(())
    })
}

//! Two levels: a checkpoint copies the pages the tail wrote into L0, in
//! Restate, as they are; once L0 holds as much as L1 (at most `l0_bytes`),
//! a checkpoint promotes instead: it rebuilds the frames those pages
//! changed into L1, in S3 if there is one, and takes L0 along.
#![cfg(feature = "simulation")]
use state::{FlushReport, Result, State, StateOptions, simulation::*};
use std::sync::Arc;

/// A page that compresses to about its size.
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

/// A checkpoint at every flush, L0 promoted past 64 KiB.
fn options() -> StateOptions {
    StateOptions {
        checkpoint_entries: 1,
        l0_bytes: 64 << 10,
        ..Default::default()
    }
}

/// Write pages `pages` of `db`, seeded by `seed`, in one flush: its report.
async fn write(state: &State, pages: std::ops::Range<u64>, seed: u64) -> Result<FlushReport> {
    let file = state.file("db")?;
    let mut mutation = state.mutation();
    for n in pages {
        mutation.write(&file, n, Some(noise(seed + n)))?;
    }
    mutation.flush().await
}

/// Whether a flush promoted; `None` if it made no checkpoint.
fn promoted(report: &FlushReport) -> Option<bool> {
    report.checkpoint.as_ref().map(|c| c.promoted)
}

/// A state of 128 KiB in S3, written to a page a flush: each checkpoint
/// copies its page into L0, with no request to S3 but the listing after the
/// promotion, until L0 holds `l0_bytes`; then one promotes, with one upload.
#[test]
fn pages_go_to_l0_until_it_holds_l0_bytes() -> Result<()> {
    futures::executor::block_on(async {
        let (store, s3) = (Arc::new(MemoryKv::default()), MemoryBlobs::default());
        let state = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(options())
            .await?;
        write(&state, 0..32, 0).await?;
        let report = write(&state, 0..1, 100).await?;
        assert_eq!(promoted(&report), Some(true), "the first checkpoint");
        assert_eq!(s3.puts(), 1);
        let requests = || (s3.puts(), s3.gets(), s3.lists());
        // The collection after the copy right after a promotion lists S3,
        // for what the promotion replaced; no later copy makes a request.
        let mut n = 1;
        write(&state, n..n + 1, 100).await?;
        n += 1;
        let mut copies = 1;
        let before = requests();
        let report = loop {
            let report = write(&state, n % 32..n % 32 + 1, 100).await?;
            if promoted(&report) == Some(true) {
                break report;
            }
            assert_eq!(requests(), before, "no request to S3 at copy {copies}");
            copies += 1;
            n += 1;
        };
        // 4 KiB a page: about 16 copies fill 64 KiB.
        assert!((12..=17).contains(&copies), "{copies} copies");
        let checkpoint = report.checkpoint.expect("a promotion");
        assert_eq!(s3.puts(), 2);
        // The pages L0 held are rebuilt into frames, not moved.
        assert!(
            checkpoint.frames > 0 && checkpoint.pages == 0,
            "{checkpoint:?}"
        );
        let file = state.file("db")?;
        for page in 0..32 {
            let seed = if page <= n { 100 } else { 0 };
            assert_eq!(
                file.read(page).await?,
                Some(noise(seed + page)),
                "page {page}"
            );
        }
        Ok(())
    })
}

/// A promotion takes L0 along: two checkpoints later, its packs are
/// collected, and a cold open reads every page from L1 and the L0 written
/// since.
#[test]
fn a_promotion_empties_l0() -> Result<()> {
    futures::executor::block_on(async {
        let (store, s3) = (Arc::new(MemoryKv::default()), MemoryBlobs::default());
        let state = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(options())
            .await?;
        write(&state, 0..32, 0).await?;
        let mut version = None;
        for n in 0..40 {
            let report = write(&state, n % 32..n % 32 + 1, 7).await?;
            if let Some(c) = report.checkpoint.filter(|c| c.promoted && n > 0) {
                version = Some(c.version);
                break;
            }
        }
        let version = version.expect("a second promotion");
        write(&state, 0..1, 9).await?;
        write(&state, 1..2, 9).await?;
        // L0 holds only packs written after the promotion.
        let written = |pack: &str| u64::from_str_radix(&pack[3..19], 16).unwrap();
        let l0: Vec<String> = (store.packs().into_iter())
            .filter(|pack| pack.starts_with("l0/"))
            .collect();
        assert!(!l0.is_empty());
        for pack in l0 {
            assert!(written(&pack) > version, "{pack} left in L0");
        }
        let cold = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(options())
            .await?;
        let file = cold.file("db")?;
        assert_eq!(file.read(0).await?, Some(noise(9)));
        assert_eq!(file.read(1).await?, Some(noise(10)));
        Ok(())
    })
}

/// A cold read of a page only L1 holds is one S3 GET; of a page in L0, a
/// get of its part in Restate and, together, its frame in S3, which it is
/// compressed against.
#[test]
fn a_cold_read_of_a_page_in_l0_fetches_its_frame_too() -> Result<()> {
    futures::executor::block_on(async {
        let (store, s3) = (Arc::new(MemoryKv::default()), MemoryBlobs::default());
        let state = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(options())
            .await?;
        write(&state, 0..64, 0).await?;
        write(&state, 64..65, 0).await?;
        // Page 1 into L0, and a flush to checkpoint it.
        write(&state, 1..2, 50).await?;
        write(&state, 65..66, 0).await?;
        let cold = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(StateOptions {
                prefetch_pages: 0,
                ..options()
            })
            .await?;
        let file = cold.file("db")?;
        let gets = s3.gets();
        store.reset_stats();
        assert_eq!(file.read(33).await?, Some(noise(33)));
        assert_eq!((s3.gets() - gets, store.stats().kind("pack").gets), (1, 0));
        let gets = s3.gets();
        assert_eq!(file.read(1).await?, Some(noise(51)));
        assert_eq!((s3.gets() - gets, store.stats().kind("pack").gets), (1, 1));
        Ok(())
    })
}

/// A promotion that fails (S3 down) fails nothing: L0 keeps its pages, the
/// flushes go on, and once S3 is back a promotion goes through.
#[test]
fn a_failed_promotion_leaves_l0_as_it_was() -> Result<()> {
    futures::executor::block_on(async {
        let (store, s3) = (Arc::new(MemoryKv::default()), MemoryBlobs::default());
        let state = State::open(store.clone())
            .checkpoints(Arc::new(s3.clone()))
            .options(options())
            .await?;
        write(&state, 0..32, 0).await?;
        write(&state, 0..1, 0).await?;
        s3.fail_after(Some(0));
        let mut failed = 0;
        for n in 0..40 {
            let report = write(&state, n % 32..n % 32 + 1, 3).await?;
            failed += report.checkpoint_failed.is_some() as usize;
        }
        assert!(failed > 0);
        s3.fail_after(None);
        let mut promoted = false;
        for n in 40..200 {
            let report = write(&state, n % 32..n % 32 + 1, 3).await?;
            promoted |= report.checkpoint.is_some_and(|c| c.promoted);
        }
        assert!(promoted, "a promotion once S3 is back");
        let file = state.file("db")?;
        for page in 0..32 {
            assert_eq!(file.read(page).await?, Some(noise(3 + page)), "page {page}");
        }
        Ok(())
    })
}

/// Without S3, L1 is in Restate too: the same levels, the same promotions.
#[test]
fn without_s3_l1_is_in_restate() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).options(options()).await?;
        write(&state, 0..32, 0).await?;
        let mut promotions = 0;
        for n in 0..60 {
            let report = write(&state, n % 32..n % 32 + 1, 5).await?;
            promotions += (promoted(&report) == Some(true)) as usize;
        }
        assert!(promotions >= 2, "{promotions} promotions");
        let packs = store.packs();
        assert!(
            packs.iter().any(|pack| pack.starts_with("l1/")),
            "{packs:?}"
        );
        let reopened = State::open(store.clone()).options(options()).await?;
        let file = reopened.file("db")?;
        for page in 0..32 {
            assert_eq!(file.read(page).await?, Some(noise(5 + page)), "page {page}");
        }
        Ok(())
    })
}

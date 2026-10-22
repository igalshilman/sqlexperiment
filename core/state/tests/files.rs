//! Paged files: lengths and holes, across flushes.
#![cfg(feature = "simulation")]
use state::{Result, State, simulation::*};
use std::sync::Arc;

fn page(n: u8) -> state::Page {
    vec![n; 4096].into()
}

/// The byte a page of [`page`] repeats, for readable assertions.
async fn byte(file: &state::File, n: u64) -> Result<Option<u8>> {
    Ok(file.read(n).await?.map(|page| page[0]))
}

/// A file shrunk twice in one WAL, then grown back: once flushed, every
/// page past the shorter length reads as zeros, also those only the first
/// shrink cut.
#[test]
fn shrinking_twice_in_a_wal_leaves_no_page_behind() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let file = state.file("db")?;
        let mut wal = state.wal();
        for n in 0..8 {
            wal.write(&file, n, Some(page(n as u8 + 1)))?;
        }
        wal.flush().await?;
        let mut wal = state.wal();
        wal.set_len(&file, 6 * 4096)?;
        wal.set_len(&file, 2 * 4096)?;
        wal.set_len(&file, 8 * 4096)?;
        assert_eq!(
            byte(&file, 7).await?,
            Some(8),
            "as flushed, until the WAL is"
        );
        wal.flush().await?;
        for n in 2..8 {
            assert_eq!(byte(&file, n).await?, None, "page {n}");
        }
        let reopened = State::open(store).await?;
        let file = reopened.file("db")?;
        for n in 2..8 {
            assert_eq!(byte(&file, n).await?, None, "page {n}, reopened");
        }
        assert_eq!(byte(&file, 1).await?, Some(2));
        Ok(())
    })
}

/// The base of the file `vm`: zeros.
struct Zeros;

impl state::Base for Zeros {
    fn id(&self) -> u64 {
        1
    }
    fn page(&self, _: u64) -> Option<&[u8]> {
        Some(&[0; 4096])
    }
}

/// A cold open fetches a small file's pages ahead, and a file with a base
/// decodes its log only once opened with it. A session that then
/// checkpoints reads the log's image, not the page key fetched before.
#[test]
fn a_checkpoint_never_exposes_a_page_fetched_under_a_based_log() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = |checkpoint_after| state::StorageOptions {
            checkpoint_after,
            inline_log_bytes: 0,
            ..state::StorageOptions::default()
        };
        // Page 1 checkpointed at its key, then changed in the log.
        for (n, checkpoint_after) in [(1, 0), (2, 8)] {
            let state = State::open_with_options(store.clone(), options(checkpoint_after)).await?;
            let mut wal = state.wal();
            wal.write(
                &state.file_with_base("vm", Arc::new(Zeros))?,
                1,
                Some(page(n)),
            )?;
            wal.flush().await?;
        }
        let state = State::open_with_options(store, options(0)).await?;
        let vm = state.file_with_base("vm", Arc::new(Zeros))?;
        assert_eq!(byte(&vm, 1).await?, Some(2));
        let mut wal = state.wal();
        wal.write(&state.file("db")?, 0, Some(page(9)))?;
        assert!(wal.flush().await?.checkpointed_pages > 0);
        assert_eq!(byte(&vm, 1).await?, Some(2), "after the checkpoint");
        Ok(())
    })
}

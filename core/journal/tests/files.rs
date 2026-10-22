//! Paged files: lengths and holes, across commits.
#![cfg(feature = "simulation")]
use journal::{Journal, Result, simulation::*};
use std::sync::Arc;

fn page(n: u8) -> journal::Page {
    vec![n; 4096].into()
}

/// The byte a page of [`page`] repeats, for readable assertions.
async fn byte(file: &journal::File, n: u64) -> Result<Option<u8>> {
    Ok(file.read(n).await?.map(|page| page[0]))
}

/// A file shrunk twice before a commit, then grown back: every page past
/// the shorter length reads as zeros, also those only the first shrink cut.
#[test]
fn shrinking_twice_before_a_commit_leaves_no_page_behind() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let journal = Journal::open(store.clone()).await?;
        let file = journal.file("db")?;
        for n in 0..8 {
            file.write(n, Some(page(n as u8 + 1)))?;
        }
        journal.commit().await?;
        file.set_len(6 * 4096)?;
        file.set_len(2 * 4096)?;
        file.set_len(8 * 4096)?;
        for n in 2..8 {
            assert_eq!(byte(&file, n).await?, None, "page {n} before the commit");
        }
        journal.commit().await?;
        let reopened = Journal::open(store).await?;
        let file = reopened.file("db")?;
        for n in 2..8 {
            assert_eq!(byte(&file, n).await?, None, "page {n}");
        }
        assert_eq!(byte(&file, 1).await?, Some(2));
        Ok(())
    })
}

/// The base of the file `vm`: zeros.
struct Zeros;

impl journal::Base for Zeros {
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
        let options = |checkpoint_after| journal::StorageOptions {
            checkpoint_after,
            inline_log_bytes: 0,
            ..journal::StorageOptions::default()
        };
        // Page 1 checkpointed at its key, then changed in the log.
        for (n, checkpoint_after) in [(1, 0), (2, 8)] {
            let journal =
                Journal::open_with_options(store.clone(), options(checkpoint_after)).await?;
            journal
                .file_with_base("vm", Arc::new(Zeros))?
                .write(1, Some(page(n)))?;
            journal.commit().await?;
        }
        let journal = Journal::open_with_options(store, options(0)).await?;
        let vm = journal.file_with_base("vm", Arc::new(Zeros))?;
        assert_eq!(byte(&vm, 1).await?, Some(2));
        journal.file("db")?.write(0, Some(page(9)))?;
        assert!(journal.commit().await?.checkpointed_pages > 0);
        assert_eq!(byte(&vm, 1).await?, Some(2), "after the checkpoint");
        Ok(())
    })
}

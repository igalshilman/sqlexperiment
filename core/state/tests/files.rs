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

/// A file shrunk twice in one mutation, then grown back: once flushed, every
/// page past the shorter length reads as zeros, also those only the first
/// shrink cut.
#[test]
fn shrinking_twice_in_a_mutation_leaves_no_page_behind() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let file = state.file("db")?;
        let mut mutation = state.mutation();
        for n in 0..8 {
            mutation.write(&file, n, Some(page(n as u8 + 1)))?;
        }
        mutation.flush().await?;
        let mut mutation = state.mutation();
        mutation.set_len(&file, 6 * 4096)?;
        mutation.set_len(&file, 2 * 4096)?;
        mutation.set_len(&file, 8 * 4096)?;
        assert_eq!(
            byte(&file, 7).await?,
            Some(8),
            "as flushed, until the mutation is"
        );
        mutation.flush().await?;
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

/// A file with a base decodes its pages only once opened with it, and
/// reads what was flushed through later flushes of other files.
#[test]
fn a_based_file_reads_its_pages_once_opened_with_its_base() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        for n in [1, 2] {
            let state = State::open(store.clone()).await?;
            let mut mutation = state.mutation();
            mutation.write(
                &state.file_with_base("vm", Arc::new(Zeros))?,
                1,
                Some(page(n)),
            )?;
            mutation.flush().await?;
        }
        let state = State::open(store.clone()).await?;
        assert!(state.file("vm")?.read(1).await.is_err(), "without its base");
        let vm = state.file_with_base("vm", Arc::new(Zeros))?;
        assert_eq!(byte(&vm, 1).await?, Some(2));
        let mut mutation = state.mutation();
        mutation.write(&state.file("db")?, 0, Some(page(9)))?;
        mutation.flush().await?;
        assert_eq!(byte(&vm, 1).await?, Some(2), "after another flush");
        Ok(())
    })
}

/// A cold sequential read of 1500 pages (as many frames), all in the tail:
/// every page reads as written, decoded from the tail the open fetched,
/// without a round trip per page.
#[test]
fn a_long_sequential_read_returns_every_page() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let options = || state::StateOptions {
            frame_pages: 1,
            ..state::StateOptions::default()
        };
        let state = State::open(Arc::new(memory.clone()))
            .options(options())
            .await?;
        let file = state.file("db")?;
        let mut mutation = state.mutation();
        for n in 0..1500 {
            mutation.write(&file, n, Some(page(n as u8 | 1)))?;
        }
        mutation.flush().await?;
        let store = Arc::new(LatencyKv::new(memory));
        let state = State::open(store.clone()).options(options()).await?;
        let file = state.file("db")?;
        for n in 0..1500 {
            assert_eq!(byte(&file, n).await?, Some(n as u8 | 1), "page {n}");
        }
        // The open fetched the tail, which holds every page.
        assert!(store.round_trips() < 1500 / 12, "{}", store.round_trips());
        Ok(())
    })
}

/// A flush refused for a write the state cannot store writes nothing, and
/// the state goes on.
#[test]
fn a_refused_flush_leaves_the_state_usable() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut mutation = state.mutation();
        mutation.write(
            &state.file_with_base("vm", Arc::new(Zeros))?,
            0,
            Some(page(1)),
        )?;
        mutation.flush().await?;
        let state = State::open(store).await?;
        let mut mutation = state.mutation();
        mutation.write(&state.file("vm")?, 1, None)?;
        let error = mutation.flush().await.unwrap_err().to_string();
        assert!(error.contains("open it with it"), "{error}");
        assert!(state.is_usable());
        let mut mutation = state.mutation();
        mutation.write(&state.file("db")?, 0, Some(page(2)))?;
        mutation.flush().await?;
        Ok(())
    })
}

/// A file with a base, opened without it, can still be emptied (as a new
/// run of another image empties the last one's memory); the checkpoints
/// after need no base.
#[test]
fn a_based_file_is_emptied_without_its_base() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut mutation = state.mutation();
        let vm = state.file_with_base("vm", Arc::new(Zeros))?;
        for n in 0..8 {
            mutation.write(&vm, n, Some(page(n as u8 + 1)))?;
        }
        mutation.checkpoint();
        mutation.flush().await?;
        let state = State::open(store.clone()).await?;
        let mut mutation = state.mutation();
        mutation.set_len(&state.file("vm")?, 0)?;
        mutation.flush().await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("run"), "next".into())?;
        mutation.checkpoint();
        let report = mutation.flush().await?;
        assert_eq!(report.checkpoint_failed, None);
        assert!(report.checkpoint.is_some());
        let state = State::open(store).await?;
        let vm = state.file_with_base("vm", Arc::new(Zeros))?;
        assert_eq!(vm.len(), 0);
        let mut mutation = state.mutation();
        mutation.write(&vm, 1, Some(page(7)))?;
        mutation.flush().await?;
        assert_eq!(byte(&vm, 1).await?, Some(7));
        Ok(())
    })
}

/// A flush without a cache gets nothing: the open before it got the head,
/// then the manifest and the whole tail (400 pages), together, in
/// one round trip; the flush is one more.
#[test]
fn a_cold_flush_gets_nothing_the_open_fetched_the_tail() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let state = State::open(Arc::new(memory.clone())).await?;
        let file = state.file("db")?;
        let mut mutation = state.mutation();
        for n in 0..400 {
            mutation.write(&file, n, Some(page(n as u8 | 1)))?;
        }
        mutation.flush().await?;
        let store = Arc::new(LatencyKv::new(memory));
        let state = State::open(store.clone()).await?;
        assert_eq!(store.round_trips(), 2);
        let file = state.file("db")?;
        store.reset();
        let mut mutation = state.mutation();
        for n in (0..400).step_by(37) {
            mutation.write(&file, n, Some(page(2)))?;
        }
        mutation.flush().await?;
        assert_eq!(store.round_trips(), 1);
        assert_eq!(byte(&file, 37).await?, Some(2));
        Ok(())
    })
}

/// Pages that do not compress, made from `seed`.
fn noise(seed: u64, pages: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..pages * 4096)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Small changes to a page, a checkpoint each flush, patch its frame rather
/// than rewrite it, and the tail compresses the next change against the
/// patched page; a frame rewritten whole is rebuilt, dropping its patch.
/// The file reads the same in the session and reopened, small enough to be
/// fetched whole on open or not.
#[test]
fn checkpoints_patch_frames_a_few_pages_change() -> Result<()> {
    futures::executor::block_on(async {
        for pages in [8, 64] {
            let store = Arc::new(MemoryKv::default());
            let options = state::StateOptions {
                checkpoint_entries: 1,
                ..Default::default()
            };
            let state = State::open(store.clone()).options(options.clone()).await?;
            let file = state.file("db")?;
            let mut image = noise(1, pages);
            let mut reports = Vec::new();
            for round in 0..12 {
                let at = 5 * 4096 + round * 8;
                image[at..at + 8].copy_from_slice(&[round as u8; 8]);
                if round == 6 {
                    image[4 * 4096..8 * 4096].copy_from_slice(&noise(2, 4));
                }
                let mut mutation = state.mutation();
                for page in 0..pages as u64 {
                    let range = page as usize * 4096..(page as usize + 1) * 4096;
                    if round == 0 || (4..8).contains(&page) && (round == 6 || page == 5) {
                        mutation.write(&file, page, Some(image[range].to_vec().into()))?;
                    }
                }
                reports.push(mutation.flush().await?);
                let reopened = State::open(store.clone()).options(options.clone()).await?;
                for file in [&file, &reopened.file("db")?] {
                    for page in 0..pages as u64 {
                        let range = page as usize * 4096..(page as usize + 1) * 4096;
                        let read = file.read(page).await?.expect("a page");
                        assert!(
                            read[..] == image[range],
                            "{pages} pages, round {round}, page {page}"
                        );
                    }
                }
            }
            let shape: Vec<Option<(usize, usize)>> = (reports.iter())
                .map(|report| (report.checkpoint.as_ref()).map(|c| (c.frames, c.patched)))
                .collect();
            // A checkpoint holds the flushes before its own: the one at
            // round 1 the file, the one at round 7 the frame round 6 rewrote.
            let mut expected = vec![None, Some((pages / 4, 0))];
            expected.extend([Some((0, 1)); 5]);
            expected.push(Some((1, 0)));
            expected.extend([Some((0, 1)); 4]);
            assert_eq!(shape, expected, "{pages} pages");
            for (round, report) in reports.iter().enumerate().skip(2) {
                if round != 6 {
                    assert!(
                        report.entry_bytes < 200,
                        "{pages} pages, round {round}: {report:?}"
                    );
                }
            }
        }
        Ok(())
    })
}

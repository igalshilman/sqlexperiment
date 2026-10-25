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

/// A file with a base decodes its chunks only once opened with it, and
/// reads what was flushed through later flushes of other files; without it,
/// whole chunks of it can be made holes.
#[test]
fn a_based_file_reads_its_deltas_once_opened_with_its_base() -> Result<()> {
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

        // Without its base, a whole chunk of it can still be made holes:
        // its base pages again.
        let state = State::open(store.clone()).await?;
        let mut mutation = state.mutation();
        for n in 0..4 {
            mutation.write(&state.file("vm")?, n, None)?;
        }
        mutation.flush().await?;
        let state = State::open(store).await?;
        let vm = state.file_with_base("vm", Arc::new(Zeros))?;
        assert_eq!(byte(&vm, 1).await?, Some(0), "its base page");
        Ok(())
    })
}

/// A cold sequential read of more chunks than a state keeps decoded: the
/// chunk a read decoded is returned, even when its readahead dropped them
/// all, and readahead grows to its widest.
#[test]
fn a_long_sequential_read_returns_every_chunk() -> Result<()> {
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
        // Readahead grows to 16 chunks a round trip.
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

/// A flush without a cache gets nothing: the open before it got the head,
/// then, for the map's leaves (100 chunks, two levels), the whole tail, in
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

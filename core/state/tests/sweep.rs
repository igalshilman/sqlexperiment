//! Garbage collection by mark and sweep ([`State::collect_garbage`]): a
//! blob unnamed at two sweeps, [`StateOptions::garbage_after`] versions
//! apart, is deleted; one a flush names again in between is not; a store
//! that cannot list is never swept; equal blobs are stored once.
//!
//! ```sh
//! cargo test --features simulation --test sweep
//! ```
#![cfg(feature = "simulation")]
use bytes::Bytes;
use futures::future::BoxFuture;
use state::{Result, State, StateOptions, kv::KvStore, simulation::*};
use std::{io, sync::Arc};

/// A record too large for the root: a blob of its own.
fn large(n: u8) -> Bytes {
    vec![n; 3000].into()
}

async fn set(state: &State, name: &str, value: Bytes) -> Result<()> {
    let mut mutation = state.mutation();
    mutation.set(&state.record(name), value)?;
    mutation.flush().await?;
    Ok(())
}

async fn write(state: &State, page: u8) -> Result<()> {
    let mut mutation = state.mutation();
    mutation.write(&state.file("f")?, 0, Some(vec![page; 4096].into()))?;
    mutation.flush().await?;
    Ok(())
}

/// A blob unnamed at a sweep, named again by a flush, which a reader opens
/// on, then unnamed again: the next sweep keeps it for the reader, as the
/// flush took it off the candidates. (Unnamed at both sweeps, it would go.)
#[test]
fn a_flush_that_names_a_candidate_again_keeps_it_for_its_readers() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        // Every chunk written whole: a page written again is its old blob.
        let options = StateOptions {
            rebase_ratio: 0.0,
            garbage_after: 2,
            prefetch_pages: 0,
            ..Default::default()
        };
        let open = || State::open(Arc::new(MonitorKv::new(store.clone()))).options(options.clone());
        let state = open().await?;
        write(&state, 1).await?;
        write(&state, 2).await?;
        let swept = state.collect_garbage().await?;
        assert_eq!((swept.deleted, swept.candidates), (0, 1), "page 1's blob");
        write(&state, 1).await?;
        let reader = open().reader().await?;
        write(&state, 2).await?;
        let swept = state.collect_garbage().await?;
        assert_eq!((swept.deleted, swept.candidates), (0, 1), "kept: {swept:?}");
        let page = reader.file("f")?.read(0).await?;
        assert_eq!(page, Some(vec![1; 4096].into()));
        Ok(())
    })
}

/// A store that cannot list its keys.
struct Unlisted(MemoryKv);

impl KvStore for Unlisted {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        self.0.get(key)
    }
    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.0.put(key, value)
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.0.delete(key)
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        self.0.sync()
    }
}

/// A sweep of a store that cannot list writes nothing, and the state goes on.
#[test]
fn a_store_that_cannot_list_is_never_swept() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let state = State::open(Arc::new(Unlisted(store.clone()))).await?;
        set(&state, "big", large(1)).await?;
        let keys = store.keys();
        let error = state.collect_garbage().await.unwrap_err().to_string();
        assert!(error.contains("cannot list"), "{error}");
        assert_eq!(store.keys(), keys);
        assert!(state.is_usable());
        set(&state, "big", large(2)).await?;
        Ok(())
    })
}

/// A reader writes nothing, so it does not sweep.
#[test]
fn a_reader_does_not_sweep() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        set(&State::open(store.clone()).await?, "big", large(1)).await?;
        let reader = State::open(store).reader().await?;
        let error = reader.collect_garbage().await.unwrap_err().to_string();
        assert!(error.contains("a reader writes nothing"), "{error}");
        Ok(())
    })
}

/// Equal values are one blob, and equal chunks too: a name is a hash.
#[test]
fn equal_blobs_are_stored_once() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let state = State::open(Arc::new(MonitorKv::new(store.clone()))).await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("a"), large(7))?;
        mutation.set(&state.record("b"), large(7))?;
        mutation.append(&state.log("turns"), large(7))?;
        let file = state.file("f")?;
        let chunk = state::StateOptions::default().chunk_pages;
        mutation.write(&file, 0, Some(vec![9; 4096].into()))?;
        mutation.write(&file, chunk, Some(vec![9; 4096].into()))?;
        let report = mutation.flush().await?;
        assert_eq!(
            store.blob_count(),
            2,
            "the value, and the chunk: {report:?}"
        );
        let state = State::open(Arc::new(store)).await?;
        assert_eq!(state.record("b").get(), large(7));
        assert_eq!(
            state.file("f")?.read(chunk).await?,
            Some(vec![9; 4096].into())
        );
        Ok(())
    })
}

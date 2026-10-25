//! The root in a key/value store, the blobs in a blob store ([`SplitStore`]):
//! only the root and the sweep go to the key/value store; an earlier attempt
//! fenced from the root (a zombie) puts the same bytes as the owner, or blobs
//! no root names; a flush that fails midway leaves the last root, and blobs
//! that sweeps delete.
//!
//! ```sh
//! cargo test --features simulation --test split
//! cargo test --features simulation,object-store --test split   # also over object_store's InMemory
//! ```
#![cfg(feature = "simulation")]
use bytes::Bytes;
use futures::future::BoxFuture;
use state::{
    Collected, ROOT_KEY, Result, State, StateOptions,
    blobs::{BlobStore, SplitStore},
    kv::KvStore,
    simulation::*,
};
use std::{io, sync::Arc};

fn page(n: u8) -> Bytes {
    vec![n; 4096].into()
}

/// A record too large for the root: a blob of its own.
fn large(n: u8) -> Bytes {
    vec![n; 3000].into()
}

/// Restate's view of an attempt it has fenced: reads work, writes fail.
struct Fenced(Arc<dyn KvStore>);

impl KvStore for Fenced {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        self.0.get(key)
    }
    fn put(&self, _: &str, _: Bytes) -> io::Result<()> {
        Err(io::Error::other("fenced"))
    }
    fn delete(&self, _: &str) -> io::Result<()> {
        Err(io::Error::other("fenced"))
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn split(root: Arc<dyn KvStore>, blobs: Arc<dyn BlobStore>) -> Arc<dyn KvStore> {
    Arc::new(SplitStore::new(root, blobs))
}

/// The blob stores the tests run on: in memory, and `object_store`'s.
fn blob_stores() -> Vec<(&'static str, Arc<dyn BlobStore>)> {
    #[allow(unused_mut)]
    let mut stores: Vec<(&'static str, Arc<dyn BlobStore>)> =
        vec![("memory", Arc::new(MemoryBlobs::default()))];
    #[cfg(feature = "object-store")]
    stores.push((
        "object_store",
        Arc::new(state::blobs::ObjectStoreBlobs::new(
            Arc::new(object_store::memory::InMemory::new()),
            "objects/42",
        )),
    ));
    stores
}

async fn read(store: Arc<dyn KvStore>) -> Result<(Bytes, Bytes)> {
    let state = State::open(store).await?;
    let page = state.file("f")?.read(0).await?.unwrap_or_default();
    Ok((page, state.record("big").get()))
}

/// Options whose sweeps delete what the previous one found unnamed one
/// version later.
fn sweep_soon() -> StateOptions {
    StateOptions {
        garbage_after: 1,
        ..Default::default()
    }
}

/// Sweep, flush a small record, and sweep again: the second sweep deletes
/// what the first found unnamed.
async fn sweep_twice(state: &State) -> Result<Collected> {
    state.collect_garbage().await?;
    let mut mutation = state.mutation();
    mutation.set(&state.record("tick"), "tock".into())?;
    mutation.flush().await?;
    state.collect_garbage().await
}

/// Flushes of files, records and logs, under the protocol monitor: the
/// key/value store holds the root and the sweep alone, the blob store every
/// blob, and a reopen reads it all; sweeps leave only what the root names.
#[test]
fn the_root_stays_in_the_kv_and_the_blobs_go_to_the_blob_store() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let kv = Arc::new(MemoryKv::default());
            let store = Arc::new(MonitorKv::new(SplitStore::new(kv.clone(), blobs.clone())));
            let state = State::open(store.clone()).options(sweep_soon()).await?;
            for n in 0..10u8 {
                let mut mutation = state.mutation();
                mutation.write(&state.file("f")?, 0, Some(page(n)))?;
                mutation.set(&state.record("big"), large(n))?;
                mutation.append(&state.log("turns"), large(n))?;
                mutation.flush().await?;
            }
            let collected = sweep_twice(&state).await?;
            assert_eq!(
                kv.keys().into_iter().collect::<Vec<_>>(),
                [ROOT_KEY, SWEEP_KEY],
                "{name}"
            );
            let listed = blobs.list("v17/blob/").await?;
            assert!(collected.deleted > 0, "{name}: {collected:?}");
            assert_eq!(listed.len(), collected.named, "{name}: {collected:?}");
            assert!(store.violations().is_empty(), "{:?}", store.violations());
            let reopened = split(kv, blobs);
            assert_eq!(read(reopened.clone()).await?, (page(9), large(9)), "{name}");
            let state = State::open(reopened).await?;
            assert_eq!(state.log("turns").entries().len(), 10, "{name}");
        }
        Ok(())
    })
}

/// A zombie (an earlier attempt Restate fenced from the root) flushes after
/// the new owner: where it writes what the owner wrote, it puts the same
/// bytes under the same name; the rest no root names, and sweeps delete it.
#[test]
fn a_zombie_flushing_after_the_owner_changes_nothing_a_root_names() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let kv: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
            let first = State::open(split(kv.clone(), blobs.clone())).await?;
            let mut mutation = first.mutation();
            mutation.write(&first.file("f")?, 0, Some(page(1)))?;
            mutation.flush().await?;
            drop(first);

            let fenced: Arc<dyn KvStore> = Arc::new(Fenced(kv.clone()));
            let zombie = State::open(split(fenced, blobs.clone())).await?;
            let owner = split(kv.clone(), blobs.clone());
            let owner = State::open(owner).options(sweep_soon()).await?;
            let mut mutation = owner.mutation();
            mutation.write(&owner.file("f")?, 0, Some(page(2)))?;
            mutation.set(&owner.record("big"), large(2))?;
            mutation.flush().await?;
            let before = blobs.list("v17/blob/").await?.len();
            let mut mutation = zombie.mutation();
            mutation.write(&zombie.file("f")?, 0, Some(page(2)))?;
            mutation.set(&zombie.record("big"), large(9))?;
            let error = mutation.flush().await.unwrap_err().to_string();
            assert!(error.contains("fenced"), "{name}: {error}");
            // The page's delta, the same as the owner's; the record's, new.
            assert_eq!(blobs.list("v17/blob/").await?.len(), before + 1, "{name}");
            assert_eq!(
                read(split(kv.clone(), blobs.clone())).await?,
                (page(2), large(2))
            );

            let collected = sweep_twice(&owner).await?;
            assert_eq!(
                collected.deleted, 2,
                "{name}: page 1's chunk, the zombie's record"
            );
            assert_eq!(
                blobs.list("v17/blob/").await?.len(),
                collected.named,
                "{name}"
            );
            assert_eq!(read(split(kv, blobs)).await?, (page(2), large(2)), "{name}");
        }
        Ok(())
    })
}

/// The blob store fails midway through a flush: the root stays as it was,
/// and the blob that landed is unnamed until sweeps delete it.
#[test]
fn a_flush_failing_midway_leaves_the_last_root_and_blobs_to_sweep() -> Result<()> {
    futures::executor::block_on(async {
        let kv = Arc::new(MemoryKv::default());
        let blobs = MemoryBlobs::default();
        let state = State::open(split(kv.clone(), Arc::new(blobs.clone()))).await?;
        let mut mutation = state.mutation();
        mutation.write(&state.file("f")?, 0, Some(page(1)))?;
        mutation.set(&state.record("big"), large(1))?;
        mutation.flush().await?;
        let named = blobs.len();

        blobs.fail_after(Some(1));
        let mut mutation = state.mutation();
        mutation.write(&state.file("f")?, 0, Some(page(2)))?;
        mutation.set(&state.record("big"), large(2))?;
        assert!(mutation.flush().await.is_err());
        blobs.fail_after(None);
        assert_eq!(blobs.len(), named + 1, "the one put that landed");

        let store = split(kv.clone(), Arc::new(blobs.clone()));
        let state = State::open(store).options(sweep_soon()).await?;
        let collected = sweep_twice(&state).await?;
        assert_eq!((collected.deleted, collected.candidates), (1, 0));
        assert_eq!(blobs.len(), named);
        assert_eq!(
            read(split(kv.clone(), Arc::new(blobs.clone()))).await?,
            (page(1), large(1))
        );
        Ok(())
    })
}

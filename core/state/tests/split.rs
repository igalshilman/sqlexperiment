//! The root in a key/value store, the blobs in a blob store ([`SplitStore`]):
//! only the root goes to the key/value store; an earlier attempt fenced from
//! the root (a zombie) never replaces a blob a root names, even when it draws
//! the same numbers; a flush that fails midway leaves the last root and
//! orphans that [`orphans`] finds.
//!
//! ```sh
//! cargo test --features simulation --test split
//! cargo test --features simulation,object-store --test split   # also over object_store's InMemory
//! ```
#![cfg(feature = "simulation")]
use bytes::Bytes;
use futures::future::BoxFuture;
use state::{
    ROOT_KEY, Result, State, StateOptions,
    blobs::{BlobStore, SplitStore, orphans},
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

/// Flushes of files, records and logs, under the protocol monitor: the
/// key/value store holds the root alone, the blob store every blob, and a
/// reopen reads it all, garbage deleted from the blob store as it goes.
#[test]
fn the_root_stays_in_the_kv_and_the_blobs_go_to_the_blob_store() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let kv = Arc::new(MemoryKv::default());
            let store = Arc::new(MonitorKv::new(SplitStore::new(kv.clone(), blobs.clone())));
            let options = StateOptions {
                garbage_after: 2,
                ..Default::default()
            };
            let state = State::open(store.clone()).options(options).await?;
            for n in 0..10u8 {
                let mut mutation = state.mutation();
                mutation.write(&state.file("f")?, 0, Some(page(n)))?;
                mutation.set(&state.record("big"), large(n))?;
                mutation.append(&state.log("turns"), large(n))?;
                mutation.flush().await?;
            }
            assert_eq!(
                kv.keys().into_iter().collect::<Vec<_>>(),
                [ROOT_KEY],
                "{name}"
            );
            let listed = blobs.list("v16/blob/").await?;
            // Named: a chunk, the record, 10 entries; then 2 flushes' garbage.
            assert_eq!(listed.len(), 16, "{name}: garbage deleted: {listed:?}");
            let reopened = split(kv, blobs);
            assert_eq!(read(reopened.clone()).await?, (page(9), large(9)), "{name}");
            let state = State::open(reopened).await?;
            assert_eq!(state.log("turns").entries().len(), 10, "{name}");
        }
        Ok(())
    })
}

/// A zombie (an earlier attempt Restate fenced from the root) flushes after
/// the new owner, drawing the same blob numbers: the blob store refuses its
/// blobs, and the owner's flush stands.
#[test]
fn a_zombie_flushing_after_the_owner_replaces_nothing() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let kv: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
            let first = State::open(split(kv.clone(), blobs.clone())).await?;
            let mut mutation = first.mutation();
            mutation.write(&first.file("f")?, 0, Some(page(1)))?;
            mutation.flush().await?;
            drop(first);

            fix_blob_jump(Some(1000));
            let fenced: Arc<dyn KvStore> = Arc::new(Fenced(kv.clone()));
            let zombie = State::open(split(fenced, blobs.clone())).await?;
            let owner = State::open(split(kv.clone(), blobs.clone())).await?;
            let mut mutation = owner.mutation();
            mutation.write(&owner.file("f")?, 0, Some(page(2)))?;
            mutation.set(&owner.record("big"), large(2))?;
            mutation.flush().await?;
            let mut mutation = zombie.mutation();
            mutation.write(&zombie.file("f")?, 0, Some(page(9)))?;
            mutation.set(&zombie.record("big"), large(9))?;
            let error = mutation.flush().await.unwrap_err().to_string();
            fix_blob_jump(None);
            let reopened = split(kv, blobs);
            assert_eq!(read(reopened).await?, (page(2), large(2)), "{name}");
            assert!(error.contains("another writer's blob"), "{name}: {error}");
        }
        Ok(())
    })
}

/// A zombie flushes first, with the numbers the owner draws: its blobs land
/// but its root does not, and the owner's flush fails rather than name
/// them. Reopened, the owner jumps elsewhere and flushes.
#[test]
fn a_zombie_flushing_first_fails_the_owners_flush_and_the_next_open_jumps() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let kv: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
            let fenced: Arc<dyn KvStore> = Arc::new(Fenced(kv.clone()));
            fix_blob_jump(Some(1000));
            let zombie = State::open(split(fenced, blobs.clone())).await?;
            let owner = State::open(split(kv.clone(), blobs.clone())).await?;
            let mut mutation = zombie.mutation();
            mutation.write(&zombie.file("f")?, 0, Some(page(9)))?;
            mutation.set(&zombie.record("big"), large(9))?;
            assert!(mutation.flush().await.is_err(), "{name}: fenced");
            let mut mutation = owner.mutation();
            mutation.write(&owner.file("f")?, 0, Some(page(2)))?;
            mutation.set(&owner.record("big"), large(2))?;
            let error = mutation.flush().await.unwrap_err().to_string();
            assert!(error.contains("another writer's blob"), "{name}: {error}");
            assert!(!owner.is_usable(), "{name}: the outcome is unknown");
            fix_blob_jump(None);

            let owner = State::open(split(kv.clone(), blobs.clone())).await?;
            assert_eq!(owner.version(), 0, "{name}: nothing landed");
            let mut mutation = owner.mutation();
            mutation.write(&owner.file("f")?, 0, Some(page(2)))?;
            mutation.set(&owner.record("big"), large(2))?;
            mutation.flush().await?;
            assert_eq!(read(split(kv, blobs)).await?, (page(2), large(2)), "{name}");
        }
        Ok(())
    })
}

/// The blob store fails midway through a flush: the root stays as it was,
/// the blobs that landed are orphans, and sweeping them changes nothing
/// that reads.
#[test]
fn a_flush_failing_midway_leaves_the_last_root_and_orphans_to_sweep() -> Result<()> {
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
        let root = kv.contents()[ROOT_KEY].clone();
        let stray = orphans(&root, &blobs).await?;
        assert_eq!(stray.len(), 1, "the one put that landed: {stray:?}");
        assert_eq!(blobs.len(), named + 1);
        for key in stray {
            blobs.delete(&key).await?;
        }
        assert_eq!(
            read(split(kv.clone(), Arc::new(blobs.clone()))).await?,
            (page(1), large(1))
        );
        assert!(orphans(&root, &blobs).await?.is_empty());
        Ok(())
    })
}

/// A put the blob store applied but did not acknowledge, retried: the same
/// bytes are this writer's own, and the sync succeeds; other bytes are not.
#[test]
fn a_blob_already_holding_the_same_bytes_is_a_retry() -> Result<()> {
    futures::executor::block_on(async {
        let blobs = MemoryBlobs::default();
        blobs.put_new("v16/blob/7", "same".into()).await?;
        blobs.put_new("v16/blob/8", "theirs".into()).await?;
        let store = SplitStore::new(Arc::new(MemoryKv::default()), Arc::new(blobs.clone()));
        store.put("v16/blob/7", "same".into())?;
        store.sync().await?;
        store.put("v16/blob/8", "mine".into())?;
        let error = store.sync().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(&store.get("v16/blob/8").await?.unwrap()[..], b"theirs");
        Ok(())
    })
}

//! The root in Restate, the blobs in a blob store ([`SplitStore`]): only
//! the root and the claim go to Restate; no two attempts ever put the same
//! blob key, a retry after a crash included; an earlier attempt that
//! Restate fenced (a zombie) never touches a blob a root names, whether it
//! claimed before it was fenced or tries after; what they leave, a sweep
//! deletes.
//!
//! ```sh
//! cargo test --features simulation --test split
//! cargo test --features simulation,object-store --test split   # also over object_store's InMemory
//! ```
#![cfg(feature = "simulation")]
use bytes::Bytes;
use state::{
    ROOT_KEY, Result, State, StateOptions,
    blobs::{BlobStore, SplitStore},
    kv::KvStore,
    simulation::*,
};
use std::sync::Arc;

fn page(n: u8) -> Bytes {
    vec![n; 4096].into()
}

/// A record too large for the root: a blob of its own.
fn large(n: u8) -> Bytes {
    vec![n; 3000].into()
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

/// An attempt's store: its view of Restate, beside `blobs`.
fn split(attempt: Arc<dyn KvStore>, blobs: &Arc<dyn BlobStore>) -> Arc<dyn KvStore> {
    Arc::new(SplitStore::new(attempt, blobs.clone()))
}

fn options() -> StateOptions {
    StateOptions {
        garbage_after: 1,
        ..StateOptions::default()
    }
}

/// Write page 0 of `f` and the record `big`.
async fn change(state: &State, n: u8) -> Result<()> {
    let mut mutation = state.mutation();
    mutation.write(&state.file("f")?, 0, Some(page(n)))?;
    mutation.set(&state.record("big"), large(n))?;
    mutation.flush().await?;
    Ok(())
}

async fn read(store: Arc<dyn KvStore>) -> Result<(Bytes, Bytes)> {
    let state = State::open(store).reader().await?;
    let page = state.file("f")?.read(0).await?.unwrap_or_default();
    Ok((page, state.record("big").get()))
}

/// Sweep everything dead: a sweep that marks, a flush that makes no
/// garbage, and a sweep that deletes. Then only what the root names is
/// stored.
async fn collect_all(state: &State) -> Result<()> {
    state.collect_garbage().await?;
    let mut tick = state.mutation();
    tick.set(&state.record("tick"), state.version().to_string().into())?;
    tick.flush().await?;
    let swept = state.collect_garbage().await?;
    assert_eq!(swept.listed - swept.deleted, swept.named, "{swept:?}");
    Ok(())
}

/// Flushes of files, records and logs, under the protocol monitor: Restate
/// holds the root and the claim alone, the blob store every blob; a reopen
/// reads it all, and sweeps leave only what the root names.
#[test]
fn the_root_and_the_claim_stay_in_restate_and_the_blobs_go_to_the_blob_store() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let restate = Restate::default();
            let store = Arc::new(MonitorKv::new(SplitStore::new(
                restate.attempt(),
                blobs.clone(),
            )));
            let state = State::open(store.clone()).options(options()).await?;
            for n in 0..10u8 {
                let mut mutation = state.mutation();
                mutation.write(&state.file("f")?, 0, Some(page(n)))?;
                mutation.set(&state.record("big"), large(n))?;
                mutation.append(&state.log("turns"), large(n))?;
                mutation.flush().await?;
                if n % 3 == 2 {
                    state.collect_garbage().await?;
                }
            }
            collect_all(&state).await?;
            assert_eq!(store.violations(), Vec::<String>::new(), "{name}");
            let keys: Vec<String> = restate.store.keys().into_iter().collect();
            assert_eq!(keys, [CLAIM_KEY, ROOT_KEY], "{name}");
            let listed = blobs.list("v17/blob/").await?;
            // A chunk, the record, and 10 log entries.
            assert_eq!(listed.len(), 12, "{name}: {listed:?}");
            let reopened = split(restate.attempt(), &blobs);
            assert_eq!(read(reopened.clone()).await?, (page(9), large(9)), "{name}");
            let state = State::open(reopened).await?;
            assert_eq!(state.log("turns").entries().len(), 10, "{name}");
        }
        Ok(())
    })
}

/// An attempt claims its numbers once, durably, before its first blob: its
/// later flushes need no claim of their own.
#[test]
fn an_attempt_claims_once_before_its_first_blob() -> Result<()> {
    futures::executor::block_on(async {
        let restate = Restate::default();
        let blobs: Arc<dyn BlobStore> = Arc::new(MemoryBlobs::default());
        let state = State::open(split(restate.attempt(), &blobs)).await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("small"), "inline".into())?;
        mutation.flush().await?;
        assert!(
            !restate.store.keys().contains(CLAIM_KEY),
            "no blob, no claim"
        );
        for n in 0..5 {
            change(&state, n).await?;
        }
        let claims = restate.store.stats().kind("claim").puts;
        assert_eq!(claims, 1, "one claim for five flushes");
        Ok(())
    })
}

/// The blob store fails midway through a flush, and the attempt with it.
/// The retry, a new attempt, numbers past what the failed one claimed: it
/// overwrites none of the blobs that landed, which a sweep deletes once a
/// root passes them.
#[test]
fn a_retry_numbers_past_what_the_failed_attempt_claimed() -> Result<()> {
    futures::executor::block_on(async {
        let restate = Restate::default();
        let memory = MemoryBlobs::default();
        let blobs: Arc<dyn BlobStore> = Arc::new(memory.clone());
        let state = State::open(split(restate.attempt(), &blobs))
            .options(options())
            .await?;
        change(&state, 1).await?;

        let before = memory.keys();
        memory.fail_after(Some(1));
        assert!(change(&state, 2).await.is_err());
        assert!(!state.is_usable(), "the attempt fails with its flush");
        memory.fail_after(None);
        let stray: Vec<String> = (memory.keys().into_iter())
            .filter(|key| !before.contains(key))
            .collect();
        assert_eq!(stray.len(), 1, "the one put that landed");

        let state = State::open(split(restate.attempt(), &blobs))
            .options(options())
            .await?;
        change(&state, 2).await?;
        assert_eq!(memory.overwrites(), 0, "no key put twice");
        assert_eq!(
            read(split(restate.attempt(), &blobs)).await?,
            (page(2), large(2))
        );
        let state = State::open(split(restate.attempt(), &blobs))
            .options(options())
            .await?;
        collect_all(&state).await?;
        assert!(
            !memory.keys().contains(&stray[0]),
            "the failed attempt's blob went"
        );
        Ok(())
    })
}

/// A zombie that tries to claim after Restate fenced it: the claim's sync
/// fails, which ends its state, and it puts no blob.
#[test]
fn a_zombie_claiming_after_it_was_fenced_puts_nothing() -> Result<()> {
    futures::executor::block_on(async {
        for (name, blobs) in blob_stores() {
            let restate = Restate::default();
            let zombie = State::open(split(restate.attempt(), &blobs)).await?;
            let owner = State::open(split(restate.attempt(), &blobs)).await?;
            let error = change(&zombie, 9).await.unwrap_err().to_string();
            assert!(error.contains("fenced"), "{name}: {error}");
            assert!(!zombie.is_usable(), "{name}");
            assert!(blobs.list("v17/").await?.is_empty(), "{name}: no blob put");
            change(&owner, 2).await?;
            assert_eq!(
                read(split(restate.attempt(), &blobs)).await?,
                (page(2), large(2)),
                "{name}"
            );
        }
        Ok(())
    })
}

/// A zombie that claimed while it ran, fenced since, flushes again within
/// its claim, before or after the owner flushes: its blobs land at its own
/// numbers, its root is refused, and the owner's blobs are untouched. A
/// sweep deletes the zombie's blobs once a root passes them.
#[test]
fn a_zombie_that_claimed_before_it_was_fenced_never_touches_the_owners_blobs() -> Result<()> {
    futures::executor::block_on(async {
        for zombie_first in [false, true] {
            let restate = Restate::default();
            let memory = MemoryBlobs::default();
            let blobs: Arc<dyn BlobStore> = Arc::new(memory.clone());
            let zombie = State::open(split(restate.attempt(), &blobs))
                .options(options())
                .await?;
            change(&zombie, 1).await?;
            let owner = State::open(split(restate.attempt(), &blobs))
                .options(options())
                .await?;
            let before = memory.puts();
            if zombie_first {
                assert!(change(&zombie, 9).await.is_err(), "fenced");
                change(&owner, 2).await?;
            } else {
                change(&owner, 2).await?;
                assert!(change(&zombie, 9).await.is_err(), "fenced");
            }
            assert!(memory.puts() >= before + 4, "both put their blobs");
            assert_eq!(memory.overwrites(), 0, "zombie first: {zombie_first}");
            let reopened = split(restate.attempt(), &blobs);
            assert_eq!(read(reopened.clone()).await?, (page(2), large(2)));
            let owner = State::open(reopened).options(options()).await?;
            collect_all(&owner).await?;
            assert_eq!(read(split(restate.attempt(), &blobs)).await?.1, large(2));
        }
        Ok(())
    })
}

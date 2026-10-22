//! Participants: one per name, a flush refused while one holds changes not
//! yet in a WAL, and idle state handed to the next state.
#![cfg(feature = "simulation")]
use state::{Result, State, StorageOptions, kv::KvStore, simulation::*};
use std::sync::Arc;

#[test]
fn a_name_takes_part_once_at_a_time() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let first = state.join("sql")?;
        assert!(state.join("sql").is_err());
        // A scoped view names its participants with its prefix.
        let _scoped = state.scoped("ext/").join("sql")?;
        drop(first);
        state.join("sql")?;
        Ok(())
    })
}

#[test]
fn a_flush_waits_for_unflushed_participants() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let participant = state.join("cache")?;
        let mut wal = state.wal();
        wal.set(&state.record("progress"), "1".into())?;
        participant.set_unflushed(true);
        let error = wal.flush().await.unwrap_err();
        assert!(error.to_string().contains("flush it first"), "{error}");
        assert!(state.is_usable(), "a refused flush writes nothing");
        participant.set_unflushed(false);
        // The refused flush took its WAL with it: a new one.
        let mut wal = state.wal();
        wal.set(&state.record("progress"), "1".into())?;
        wal.flush().await?;
        let reopened = State::open(store).await?;
        assert_eq!(&reopened.record("progress").get()[..], b"1");
        Ok(())
    })
}

#[test]
fn idle_state_goes_to_the_next_state_if_nothing_changed() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let options = || StorageOptions {
            cache: Some(shared_cache().namespace("idle-state")),
            ..StorageOptions::default()
        };
        let state = State::open_with_options(store.clone(), options()).await?;
        let mut wal = state.wal();
        wal.set(&state.record("n"), "1".into())?;
        wal.flush().await?;
        let engine = state.join("engine")?;
        engine.park(7u32, 1).unwrap();
        drop((engine, state));

        let state = State::open_with_options(store.clone(), options()).await?;
        let engine = state.join("engine")?;
        assert_eq!(engine.take_idle::<u32>(), Some(7));
        // A WAL changes nothing until it flushes: the state is still at the
        // last durable flush, before and after.
        let mut wal = state.wal();
        wal.set(&state.record("n"), "2".into())?;
        assert!(engine.can_park());
        wal.flush().await?;
        engine.park(9u32, 1).unwrap();
        drop((engine, state));

        // Another state of the owner flushes: what was idle is stale.
        let state = State::open_with_options(store.clone(), options()).await?;
        let other = State::open_with_options(store.clone(), options()).await?;
        let mut wal = other.wal();
        wal.set(&other.record("n"), "3".into())?;
        wal.flush().await?;
        drop((state, other));
        let state = State::open_with_options(store, options()).await?;
        assert_eq!(state.join("engine")?.take_idle::<u32>(), None);
        Ok(())
    })
}

/// Without a cache nothing is kept: `park` gives the value back.
#[test]
fn idle_state_is_given_back_without_a_cache() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let engine = state.join("engine")?;
        assert!(!engine.can_park());
        assert_eq!(engine.park(8u32, 1), Err(8));
        Ok(())
    })
}

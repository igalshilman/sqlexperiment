//! Parking: a value left for the namespace's next state, kept only while
//! nothing changed since.
#![cfg(feature = "simulation")]
use state::{Result, State, kv::KvStore, simulation::*};
use std::sync::Arc;

#[test]
fn a_parked_value_goes_to_the_next_state_if_nothing_changed() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let open = || State::open(store.clone()).cached(shared_cache(), "parking");
        let state = open().await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("n"), "1".into())?;
        mutation.flush().await?;
        state.park("engine", 7u32, 1).unwrap();
        drop(state);

        let state = open().await?;
        assert_eq!(state.take_parked::<u32>("other"), None, "another key");
        assert_eq!(state.take_parked::<u32>("engine"), Some(7));
        assert_eq!(state.take_parked::<u32>("engine"), None, "taken");
        // A mutation changes nothing until it flushes: the state is still at the
        // last durable flush, before and after.
        let mut mutation = state.mutation();
        mutation.set(&state.record("n"), "2".into())?;
        assert!(state.can_park());
        mutation.flush().await?;
        state.park("engine", 9u32, 1).unwrap();
        drop(state);

        // Another state of the owner flushes: what was parked is stale.
        let state = open().await?;
        let other = open().await?;
        let mut mutation = other.mutation();
        mutation.set(&other.record("n"), "3".into())?;
        mutation.flush().await?;
        drop((state, other));
        let state = open().await?;
        assert_eq!(state.take_parked::<u32>("engine"), None);
        Ok(())
    })
}

/// A scoped view parks under its prefix, and a value of another type is not
/// taken.
#[test]
fn a_parked_value_is_taken_by_key_and_type() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let open = || State::open(store.clone()).cached(shared_cache(), "parking-keys");
        let state = open().await?;
        state.scoped("ext/").park("engine", 1u32, 1).unwrap();
        state.park("engine", 2u32, 1).unwrap();
        drop(state);
        let state = open().await?;
        assert_eq!(state.take_parked::<u64>("engine"), None);
        assert_eq!(state.scoped("ext/").take_parked::<u32>("engine"), Some(1));
        Ok(())
    })
}

/// Without a cache nothing is kept: `park` gives the value back.
#[test]
fn a_value_is_given_back_without_a_cache() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        assert!(!state.can_park());
        assert_eq!(state.park("engine", 8u32, 1), Err(8));
        Ok(())
    })
}

/// Two handles are equal when they name the same file of the same state.
#[test]
fn files_are_equal_by_state_and_name() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        assert!(state.file("db")? == state.file("db")?);
        assert!(state.file("db")? != state.file("other")?);
        let reader = State::open(store).reader().await?;
        assert!(state.file("db")? != reader.file("db")?);
        Ok(())
    })
}

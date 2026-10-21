//! Participants: one per name, a commit refused while one holds changes not
//! yet in the journal, and idle state handed to the next journal.
#![cfg(feature = "simulation")]
use journal::{Journal, Result, StorageOptions, kv::KvStore, simulation::*};
use std::sync::Arc;

#[test]
fn a_name_takes_part_once_at_a_time() -> Result<()> {
    futures::executor::block_on(async {
        let journal = Journal::open(Arc::new(MemoryKv::default())).await?;
        let first = journal.join("sql")?;
        assert!(journal.join("sql").is_err());
        // A scoped view names its participants with its prefix.
        let _scoped = journal.scoped("ext/").join("sql")?;
        drop(first);
        journal.join("sql")?;
        Ok(())
    })
}

#[test]
fn a_commit_waits_for_unflushed_participants() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let journal = Journal::open(store.clone()).await?;
        let participant = journal.join("cache")?;
        journal.record("progress").set("1".into());
        participant.set_unflushed(true);
        let error = journal.commit().await.unwrap_err();
        assert!(error.to_string().contains("flush it first"), "{error}");
        assert!(journal.is_usable(), "a refused commit writes nothing");
        participant.set_unflushed(false);
        journal.commit().await?;
        let reopened = Journal::open(store).await?;
        assert_eq!(&reopened.record("progress").get()[..], b"1");
        Ok(())
    })
}

#[test]
fn idle_state_goes_to_the_next_journal_if_nothing_changed() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let options = || StorageOptions {
            cache: Some(shared_cache().namespace("idle-state")),
            ..StorageOptions::default()
        };
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        journal.record("n").set("1".into());
        journal.commit().await?;
        let engine = journal.join("engine")?;
        engine.park(7u32, 1).unwrap();
        drop((engine, journal));

        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let engine = journal.join("engine")?;
        assert_eq!(engine.take_idle::<u32>(), Some(7));
        // Pending changes: the state no longer matches what it would keep.
        journal.record("n").set("2".into());
        assert!(!engine.can_park());
        assert_eq!(engine.park(8u32, 1), Err(8));
        journal.commit().await?;
        engine.park(9u32, 1).unwrap();
        drop((engine, journal));

        // Another journal of the owner commits: what was idle is stale.
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let other = Journal::open_with_options(store.clone(), options()).await?;
        other.record("n").set("3".into());
        other.commit().await?;
        drop((journal, other));
        let journal = Journal::open_with_options(store, options()).await?;
        assert_eq!(journal.join("engine")?.take_idle::<u32>(), None);
        Ok(())
    })
}

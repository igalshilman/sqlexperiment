//! Participants: one per name, a commit refused while one holds changes not
//! yet in the volume, and idle state handed to the next volume.
#![cfg(feature = "simulation")]
use std::sync::Arc;
use volume::{Result, StorageOptions, Volume, kv::KvStore, simulation::*};

#[test]
fn a_name_takes_part_once_at_a_time() -> Result<()> {
    futures::executor::block_on(async {
        let volume = Volume::open(Arc::new(MemoryKv::default())).await?;
        let first = volume.join("sql")?;
        assert!(volume.join("sql").is_err());
        // A scoped view names its participants with its prefix.
        let _scoped = volume.scoped("ext/").join("sql")?;
        drop(first);
        volume.join("sql")?;
        Ok(())
    })
}

#[test]
fn a_commit_waits_for_unflushed_participants() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open(store.clone()).await?;
        let participant = volume.join("cache")?;
        volume.record("progress").set("1".into());
        participant.set_unflushed(true);
        let error = volume.commit().await.unwrap_err();
        assert!(error.to_string().contains("flush it first"), "{error}");
        assert!(volume.is_usable(), "a refused commit writes nothing");
        participant.set_unflushed(false);
        volume.commit().await?;
        let reopened = Volume::open(store).await?;
        assert_eq!(&reopened.record("progress").get()[..], b"1");
        Ok(())
    })
}

#[test]
fn idle_state_goes_to_the_next_volume_if_nothing_changed() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let options = || StorageOptions {
            cache: Some(shared_cache().namespace("idle-state")),
            ..StorageOptions::default()
        };
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        volume.record("n").set("1".into());
        volume.commit().await?;
        let engine = volume.join("engine")?;
        engine.park(7u32, 1).unwrap();
        drop((engine, volume));

        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let engine = volume.join("engine")?;
        assert_eq!(engine.take_idle::<u32>(), Some(7));
        // Pending changes: the state no longer matches what it would keep.
        volume.record("n").set("2".into());
        assert!(!engine.can_park());
        assert_eq!(engine.park(8u32, 1), Err(8));
        volume.commit().await?;
        engine.park(9u32, 1).unwrap();
        drop((engine, volume));

        // Another volume of the owner commits: what was idle is stale.
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let other = Volume::open_with_options(store.clone(), options()).await?;
        other.record("n").set("3".into());
        other.commit().await?;
        drop((volume, other));
        let volume = Volume::open_with_options(store, options()).await?;
        assert_eq!(volume.join("engine")?.take_idle::<u32>(), None);
        Ok(())
    })
}

//! A commit: refused before it writes what it cannot store, one at a time,
//! and, cancelled midway, an unknown outcome that no other volume of the
//! owner commits on top of.
#![cfg(feature = "simulation")]
use bytes::Bytes;
use futures::{FutureExt, future::BoxFuture};
use std::{io, sync::Arc};
use volume::{MAX_RECORD_BYTES, Result, StorageOptions, Volume, kv::KvStore, simulation::*};

/// A store whose sync never completes: a commit waits on it forever.
struct Stalled(Arc<MemoryKv>);

impl KvStore for Stalled {
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
        Box::pin(futures::future::pending())
    }
}

#[test]
fn a_commit_refuses_what_it_cannot_store_and_the_volume_goes_on() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open(store.clone()).await?;
        volume.record("kept").set("yes".into());
        volume
            .record("large")
            .set(vec![0; MAX_RECORD_BYTES + 1].into());
        let error = volume.commit().await.unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");
        assert!(volume.is_usable());
        assert!(store.keys().is_empty(), "nothing written");
        // What was pending stays: fixed, it commits.
        volume.record("large").set("small".into());
        volume.commit().await?;
        // A length a commit cannot store is refused where it is set.
        assert!(volume.file("f")?.set_len(1000).is_err());
        let reopened = Volume::open(store).await?;
        assert_eq!(&reopened.record("kept").get()[..], b"yes");
        assert_eq!(&reopened.record("large").get()[..], b"small");
        Ok(())
    })
}

#[test]
fn a_volume_commits_one_at_a_time() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Stalled(Arc::new(MemoryKv::default())));
        // With a cache, an open needs no sync, which this store never completes.
        let options = StorageOptions {
            cache: Some(fresh_namespace()),
            ..StorageOptions::default()
        };
        let volume = Volume::open_with_options(store, options).await?;
        volume.record("n").set("1".into());
        let mut first = Box::pin(volume.commit());
        assert!((&mut first).now_or_never().is_none(), "waits on the sync");
        let clone = volume.clone();
        clone.record("n").set("2".into());
        let error = clone.commit().await.unwrap_err().to_string();
        assert!(error.contains("running"), "{error}");
        Ok(())
    })
}

#[test]
fn a_cancelled_commit_ends_the_volume_and_no_other_commits_on_top() -> Result<()> {
    futures::executor::block_on(async {
        let memory = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StorageOptions {
            cache: Some(cache.clone()),
            ..StorageOptions::default()
        };
        let stalled: Arc<dyn KvStore> = Arc::new(Stalled(memory.clone()));
        let volume = Volume::open_with_options(stalled, options()).await?;
        // Another volume of the owner, on the same state.
        let other = Volume::open_with_options(memory.clone(), options()).await?;
        volume.record("n").set("1".into());
        let mut commit = Box::pin(volume.commit());
        assert!((&mut commit).now_or_never().is_none());
        // Dropped midway: its record may or may not be stored.
        drop(commit);
        assert!(!volume.is_usable());
        other.record("n").set("2".into());
        let error = other.commit().await.unwrap_err().to_string();
        assert!(error.contains("reopen"), "{error}");
        Ok(())
    })
}

#[test]
fn idle_state_heavier_than_the_budget_is_not_kept_and_evicts_nothing() -> Result<()> {
    futures::executor::block_on(async {
        let cache = open_cache(volume::PageCacheConfig {
            idle_bytes: 100,
            ..cache_config(1 << 20, 4 << 20)
        });
        let stores: Vec<Arc<dyn KvStore>> =
            vec![Arc::new(MemoryKv::default()), Arc::new(MemoryKv::default())];
        let options = |namespace: &str| StorageOptions {
            cache: Some(cache.namespace(namespace)),
            ..StorageOptions::default()
        };
        for ((namespace, value, weight), store) in [("light", 1u32, 10), ("heavy", 2, 101)]
            .into_iter()
            .zip(&stores)
        {
            let volume = Volume::open_with_options(store.clone(), options(namespace)).await?;
            volume.record("n").set("1".into());
            volume.commit().await?;
            volume.join("engine")?.park(value, weight).unwrap();
        }
        // The light one is still idle; the heavy one was never kept.
        for ((namespace, expected), store) in [("light", Some(1u32)), ("heavy", None)]
            .into_iter()
            .zip(&stores)
        {
            let volume = Volume::open_with_options(store.clone(), options(namespace)).await?;
            assert_eq!(
                volume.join("engine")?.take_idle::<u32>(),
                expected,
                "{namespace}"
            );
        }
        Ok(())
    })
}

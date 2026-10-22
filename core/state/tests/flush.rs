//! WALs and their flushes: a WAL is read once flushed, discarded when
//! dropped, and refused by another state; a write refuses, before the WAL
//! changes, what no flush could store; a flush is one at a time, refuses a
//! WAL the directory no longer agrees with, and, cancelled midway, leaves an
//! unknown outcome that no other state of the owner flushes on top of.
#![cfg(feature = "simulation")]
use bytes::Bytes;
use futures::{FutureExt, future::BoxFuture};
use state::{
    MAX_NAME_BYTES, MAX_RECORD_BYTES, Page, Result, State, StorageOptions, kv::KvStore,
    simulation::*,
};
use std::{io, sync::Arc};

fn page(n: u8) -> Page {
    vec![n; 4096].into()
}

/// A store whose sync never completes: a flush waits on it forever.
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

/// The state reads as last flushed, whatever a WAL holds, until that WAL
/// flushes; a second WAL flushes on top of the first.
#[test]
fn a_wal_is_read_once_flushed() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let (n, db) = (state.record("n"), state.file("db")?);
        let mut wal = state.wal();
        n.set(&mut wal, "1".into())?;
        db.write(&mut wal, 0, Some(page(1)))?;
        assert!(n.get().is_empty() && db.is_empty(), "not flushed yet");
        assert_eq!(db.read(0).await?, None);
        wal.flush().await?;
        assert_eq!(&n.get()[..], b"1");
        assert_eq!(db.read(0).await?, Some(page(1)));
        // A second WAL, on top.
        let mut wal = state.wal();
        n.set(&mut wal, "2".into())?;
        db.write(&mut wal, 1, Some(page(2)))?;
        assert_eq!((&n.get()[..], db.len()), (&b"1"[..], 4096));
        wal.flush().await?;
        assert_eq!((&n.get()[..], db.len()), (&b"2"[..], 8192));
        assert_eq!(db.read(0).await?, Some(page(1)));
        assert_eq!(db.read(1).await?, Some(page(2)));
        assert_eq!(state.version(), 2);
        let reopened = State::open(store).await?;
        assert_eq!(&reopened.record("n").get()[..], b"2");
        assert_eq!(reopened.file("db")?.read(1).await?, Some(page(2)));
        Ok(())
    })
}

/// A WAL dropped discards its writes: nothing is written, the state reads
/// as last flushed, and the next WAL flushes.
#[test]
fn a_wal_dropped_discards_its_writes() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut wal = state.wal();
        state.record("n").set(&mut wal, "1".into())?;
        wal.flush().await?;
        let keys = store.keys();
        let mut wal = state.wal();
        state.record("n").set(&mut wal, "2".into())?;
        state.file("db")?.write(&mut wal, 0, Some(page(1)))?;
        assert!(!wal.is_empty());
        drop(wal);
        assert_eq!(&state.record("n").get()[..], b"1");
        assert!(state.file("db")?.is_empty());
        assert_eq!(state.version(), 1);
        assert_eq!(store.keys(), keys, "nothing written");
        let mut wal = state.wal();
        state.record("n").set(&mut wal, "3".into())?;
        wal.flush().await?;
        let reopened = State::open(store).await?;
        assert_eq!(&reopened.record("n").get()[..], b"3");
        assert!(reopened.file("db")?.is_empty());
        Ok(())
    })
}

/// A receipt is pending until its WAL flushes (flushed, an empty one too)
/// or is dropped unflushed (discarded).
#[test]
fn a_receipt_reads_flushed_after_the_flush_and_discarded_after_a_drop() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let mut wal = state.wal();
        let receipt = wal.receipt();
        state.record("n").set(&mut wal, "1".into())?;
        assert!(!receipt.is_flushed() && !receipt.is_discarded(), "pending");
        wal.flush().await?;
        assert!(receipt.is_flushed());
        let mut wal = state.wal();
        let (first, second) = (wal.receipt(), wal.receipt());
        state.record("n").set(&mut wal, "2".into())?;
        drop(wal);
        assert!(first.is_discarded() && second.is_discarded());
        let mut empty = state.wal();
        let receipt = empty.receipt();
        empty.flush().await?;
        assert!(receipt.is_flushed(), "nothing written, nothing discarded");
        Ok(())
    })
}

/// Every write refuses a WAL of another state, and changes nothing; a
/// scoped view's WAL is the state's own.
#[test]
fn a_wal_of_another_state_is_refused() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let other = State::open(Arc::new(MemoryKv::default())).await?;
        let mut wal = other.wal();
        let error = state.record("n").set(&mut wal, "1".into()).unwrap_err();
        assert!(error.to_string().contains("another state"), "{error}");
        let db = state.file("db")?;
        assert!(db.write(&mut wal, 0, Some(page(1))).is_err());
        assert!(db.set_len(&mut wal, 0).is_err());
        assert!(wal.is_empty());
        let mut wal = state.scoped("ext/").wal();
        state.record("n").set(&mut wal, "1".into())?;
        state.scoped("ext/").record("n").set(&mut wal, "2".into())?;
        wal.flush().await?;
        assert_eq!(&state.record("n").get()[..], b"1");
        assert_eq!(&state.record("ext/n").get()[..], b"2");
        Ok(())
    })
}

/// A write refuses what no flush could store (a record or a name too long,
/// a bad length, a page past the largest file or not 4096 bytes) before the
/// WAL changes: it holds what was accepted, and flushes.
#[test]
fn a_write_refuses_what_no_flush_could_store_and_the_wal_goes_on() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut wal = state.wal();
        state.record("kept").set(&mut wal, "yes".into())?;
        let large = vec![0; MAX_RECORD_BYTES + 1].into();
        let error = state.record("large").set(&mut wal, large).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        let long = "n".repeat(MAX_NAME_BYTES + 1);
        let error = state.record(&long).set(&mut wal, "1".into()).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert!(state.file(&long)?.write(&mut wal, 0, None).is_err());
        let file = state.file("f")?;
        assert!(file.set_len(&mut wal, 1000).is_err());
        assert!(file.write(&mut wal, u64::MAX, None).is_err());
        assert!(file.write(&mut wal, 0, Some(vec![0; 100].into())).is_err());
        // Fixed, it flushes: what was accepted, and nothing of the rest.
        state.record("large").set(&mut wal, "small".into())?;
        wal.flush().await?;
        let reopened = State::open(store).await?;
        assert_eq!(&reopened.record("kept").get()[..], b"yes");
        assert_eq!(&reopened.record("large").get()[..], b"small");
        assert!(reopened.record(&long).get().is_empty());
        assert!(reopened.file("f")?.is_empty());
        Ok(())
    })
}

/// A name is a record's or a paged file's, in the WAL and once flushed: a
/// write as the other is refused.
#[test]
fn a_name_is_a_record_or_a_paged_file() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let mut wal = state.wal();
        state.file("db")?.write(&mut wal, 0, Some(page(1)))?;
        state.record("outbox").set(&mut wal, "1".into())?;
        let error = state.record("db").set(&mut wal, "2".into()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("is a paged file, not a record"), "{error}");
        let error = state.file("outbox")?.write(&mut wal, 0, None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("is a record, not a paged file"), "{error}");
        wal.flush().await?;
        let mut wal = state.wal();
        assert!(state.record("db").set(&mut wal, "2".into()).is_err());
        assert!(state.file("outbox")?.set_len(&mut wal, 0).is_err());
        assert!(wal.is_empty());
        Ok(())
    })
}

/// A WAL whose record another WAL's flush made a paged file: its flush is
/// refused before it writes, and the state goes on.
#[test]
fn a_flush_refuses_a_wal_the_directory_no_longer_agrees_with() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut stale = state.wal();
        state.record("x").set(&mut stale, "record".into())?;
        let mut other = state.wal();
        state.file("x")?.write(&mut other, 0, Some(page(1)))?;
        other.flush().await?;
        let keys = store.keys();
        let error = stale.flush().await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("is a paged file, not a record"), "{error}");
        assert!(state.is_usable());
        assert_eq!(store.keys(), keys, "nothing written");
        assert_eq!(state.file("x")?.read(0).await?, Some(page(1)));
        Ok(())
    })
}

/// A WAL kept while another flushes still flushes afterwards: its writes
/// land on top, and a file the other flush grew keeps its length and pages.
#[test]
fn a_wal_kept_across_another_flush_flushes_on_top() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut kept = state.wal();
        state.record("n").set(&mut kept, "kept".into())?;
        state.record("a").set(&mut kept, "kept".into())?;
        state.file("db")?.write(&mut kept, 0, Some(page(1)))?;
        let mut other = state.wal();
        state.record("n").set(&mut other, "other".into())?;
        state.record("b").set(&mut other, "other".into())?;
        state.file("db")?.write(&mut other, 3, Some(page(2)))?;
        other.flush().await?;
        assert_eq!(&state.record("n").get()[..], b"other");
        kept.flush().await?;
        assert_eq!(state.version(), 2);
        let reopened = State::open(store).await?;
        for (name, value) in [("n", "kept"), ("a", "kept"), ("b", "other")] {
            assert_eq!(&reopened.record(name).get()[..], value.as_bytes(), "{name}");
        }
        let db = reopened.file("db")?;
        assert_eq!(db.len(), 4 * 4096);
        assert_eq!(db.read(0).await?, Some(page(1)));
        assert_eq!(db.read(3).await?, Some(page(2)));
        Ok(())
    })
}

/// A length set in a WAL holds at its flush, whatever another flush grew
/// the file to meanwhile: the pages past it are gone, and regrowing the file
/// reads zeros there.
#[test]
fn a_length_set_in_a_kept_wal_cuts_what_another_flush_grew() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let mut kept = state.wal();
        state.file("db")?.write(&mut kept, 0, Some(page(1)))?;
        state.file("db")?.set_len(&mut kept, 4096)?;
        let mut other = state.wal();
        state.file("db")?.write(&mut other, 3, Some(page(2)))?;
        other.flush().await?;
        kept.flush().await?;
        assert_eq!(state.file("db")?.len(), 4096);
        let mut regrown = state.wal();
        state.file("db")?.write(&mut regrown, 5, Some(page(3)))?;
        regrown.flush().await?;
        let reopened = State::open(store).await?;
        let db = reopened.file("db")?;
        assert_eq!(db.read(0).await?, Some(page(1)));
        assert_eq!(db.read(3).await?, None);
        assert_eq!(db.read(5).await?, Some(page(3)));
        Ok(())
    })
}

#[test]
fn a_state_flushes_one_at_a_time() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Stalled(Arc::new(MemoryKv::default())));
        // With a cache, an open needs no sync, which this store never completes.
        let options = StorageOptions {
            cache: Some(fresh_namespace()),
            ..StorageOptions::default()
        };
        let state = State::open_with_options(store, options).await?;
        let mut wal = state.wal();
        state.record("n").set(&mut wal, "1".into())?;
        let mut first = Box::pin(wal.flush());
        assert!((&mut first).now_or_never().is_none(), "waits on the sync");
        let clone = state.clone();
        let mut second = clone.wal();
        clone.record("n").set(&mut second, "2".into())?;
        let error = second.flush().await.unwrap_err().to_string();
        assert!(error.contains("running"), "{error}");
        assert!(state.is_usable(), "a refused flush ends nothing");
        Ok(())
    })
}

#[test]
fn a_cancelled_flush_ends_the_state_and_no_other_flushes_on_top() -> Result<()> {
    futures::executor::block_on(async {
        let memory = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StorageOptions {
            cache: Some(cache.clone()),
            ..StorageOptions::default()
        };
        let stalled: Arc<dyn KvStore> = Arc::new(Stalled(memory.clone()));
        let state = State::open_with_options(stalled, options()).await?;
        // Another state of the owner, opened on the same flush.
        let other = State::open_with_options(memory.clone(), options()).await?;
        let mut wal = state.wal();
        let receipt = wal.receipt();
        state.record("n").set(&mut wal, "1".into())?;
        let mut flush = Box::pin(wal.flush());
        assert!((&mut flush).now_or_never().is_none());
        // Dropped midway: its record may or may not be stored.
        drop(flush);
        assert!(!state.is_usable());
        assert!(!receipt.is_flushed() && !receipt.is_discarded(), "unknown");
        let error = state.wal().flush().await.unwrap_err().to_string();
        assert!(error.contains("ended"), "{error}");
        let mut wal = other.wal();
        other.record("n").set(&mut wal, "2".into())?;
        let error = wal.flush().await.unwrap_err().to_string();
        assert!(error.contains("reopen"), "{error}");
        Ok(())
    })
}

#[test]
fn idle_state_heavier_than_the_budget_is_not_kept_and_evicts_nothing() -> Result<()> {
    futures::executor::block_on(async {
        let cache = open_cache(state::PageCacheConfig {
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
            let state = State::open_with_options(store.clone(), options(namespace)).await?;
            let mut wal = state.wal();
            state.record("n").set(&mut wal, "1".into())?;
            wal.flush().await?;
            state.join("engine")?.park(value, weight).unwrap();
        }
        // The light one is still idle; the heavy one was never kept.
        for ((namespace, expected), store) in [("light", Some(1u32)), ("heavy", None)]
            .into_iter()
            .zip(&stores)
        {
            let state = State::open_with_options(store.clone(), options(namespace)).await?;
            assert_eq!(
                state.join("engine")?.take_idle::<u32>(),
                expected,
                "{namespace}"
            );
        }
        Ok(())
    })
}

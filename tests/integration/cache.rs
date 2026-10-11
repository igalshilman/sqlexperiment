use sqlite_kv::{
    PageCache, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv},
};
use std::sync::Arc;

fn cached(cache: &PageCache) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        ..StorageOptions::default()
    }
}

async fn create(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<()> {
    let mut db = Session::open_with_options(store, options).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
        .await?;
    // Incompressible: a large save that checkpoints itself through the cache.
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<400) INSERT INTO t SELECT x,randomblob(900) FROM n").await?;
    db.save().await?;
    Ok(())
}

async fn payload_sum(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<i64> {
    let mut db = Session::open_with_options(store, options).await?;
    let rows = db.query("SELECT sum(length(payload) + id) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn an_owner_cache_serves_reopened_sessions_in_one_round_trip() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = PageCache::new(64 << 20);
        create(store.clone(), cached(&cache)).await?;
        assert!(cache.bytes() > 0, "a checkpoint writes through the cache");
        let expected = 400 * 900 + 400 * 401 / 2;
        // The first session confirms nothing it wrote; it reads the full table.
        store.reset();
        store.inner.reset_stats();
        assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
        let stats = store.inner.stats();
        assert_eq!(store.round_trips(), 1, "meta and probe together: {stats:?}");
        assert_eq!((stats.kind("page").gets, stats.puts), (0, 0));

        store.inner.reset_stats();
        assert_eq!(
            payload_sum(store.clone(), StorageOptions::default()).await?,
            expected
        );
        assert!(store.inner.stats().kind("page").gets > 0);
        Ok(())
    })
}

/// Another writer that checkpoints changes `meta`, and the owner refreshes or
/// drops its cache; one that only appends is overridden by the log.
#[test]
fn other_writers_never_leave_the_cache_stale() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let cache = PageCache::new(64 << 20);
        create(store.clone(), cached(&cache)).await?;
        let mut expected = 400 * 900 + 400 * 401 / 2;
        assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
        for (round, checkpoint_after) in [(0, 0), (1, 100), (2, 100), (3, 0)] {
            let other = StorageOptions {
                checkpoint_after,
                ..StorageOptions::default()
            };
            let mut db = Session::open_with_options(store.clone(), other).await?;
            db.execute(&format!(
                "UPDATE t SET payload = printf('%0800d', id) WHERE id % 4 = {round}"
            ))
            .await?;
            db.save().await?;
            expected -= 100 * 100;
            assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
        }
        Ok(())
    })
}

fn owner(cache: &PageCache) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        ..StorageOptions::default()
    }
}
async fn count(db: &mut Session) -> Result<i64> {
    Ok(db.query("SELECT count(*) FROM t").await?[0][0]
        .as_int()
        .unwrap())
}

/// A clean save or read-only session leaves its engine to the next session; a
/// session with unsaved changes, an error, or a write by anyone else does not.
#[test]
fn warm_engines_continue_only_from_an_unchanged_committed_state() -> Result<()> {
    use sqlite_kv::simulation::{Effect, Failpoint, FaultKv, Operation};
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let cache = PageCache::new(1 << 20);
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine());
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").await?;
        db.save().await?;

        // After a save, and after a read-only session, the engine continues.
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine(), "after a save");
        db.execute("INSERT INTO t VALUES (1)").await?;
        db.save().await?;
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine());
        assert_eq!(count(&mut db).await?, 1);
        drop(db);
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine(), "after a read-only session");

        // Unsaved changes discard the engine with them.
        db.execute("INSERT INTO t VALUES (2)").await?;
        drop(db);
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine(), "unsaved changes");
        assert_eq!(count(&mut db).await?, 1);
        drop(db);

        // So does a query error.
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine());
        assert!(db.execute("INSERT INTO missing VALUES (1)").await.is_err());
        drop(db);
        let db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine(), "poisoned session");
        drop(db);

        // Another writer appends, then checkpoints: both force a fresh engine.
        for checkpoint_after in [100, 0] {
            let other = StorageOptions {
                checkpoint_after,
                ..StorageOptions::default()
            };
            let mut db = Session::open_with_options(store.clone(), other).await?;
            db.execute("INSERT INTO t SELECT max(id) + 1 FROM t")
                .await?;
            db.save().await?;
            let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
            assert!(!db.reused_engine(), "another writer");
            let expected = if checkpoint_after == 0 { 3 } else { 2 };
            assert_eq!(count(&mut db).await?, expected);
            drop(db);
            let db = Session::open_with_options(store.clone(), owner(&cache)).await?;
            assert!(db.reused_engine(), "warm again once unchanged");
        }

        // A save that fails before committing leaves nothing to reuse.
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        db.execute("INSERT INTO t VALUES (100)").await?;
        let _gate = store.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: "v12/log/".into(),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        assert!(db.save().await.is_err());
        store.disarm();
        let mut db = Session::open_with_options(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine(), "failed save");
        assert_eq!(count(&mut db).await?, 3);
        Ok(())
    })
}

/// A brief other owner that checkpoints leaves this owner's cache warm: open
/// refreshes the pages it checkpointed from its still present log segments. Once
/// garbage collection has deleted them, the cache drops its pages.
#[test]
fn a_brief_other_owner_leaves_the_cache_warm() -> Result<()> {
    futures::executor::block_on(async {
        for (saves, warm) in [(6, true), (12, false)] {
            let store = Arc::new(LatencyKv::new(MemoryKv::default()));
            let cache = PageCache::new(64 << 20);
            create(store.clone(), cached(&cache)).await?;
            let other = PageCache::new(64 << 20);
            for n in 0..saves {
                let mut db = Session::open_with_options(store.clone(), cached(&other)).await?;
                db.execute(&format!(
                    "UPDATE t SET payload = printf('%0800d', id) WHERE id = {}",
                    n * 31 + 1
                ))
                .await?;
                db.save().await?;
            }
            store.inner.reset_stats();
            let expected = 400 * 900 + 400 * 401 / 2 - 100 * saves;
            assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
            let page_gets = store.inner.stats().kind("page").gets;
            assert_eq!(page_gets == 0, warm, "{saves} saves: {page_gets} page gets");
        }
        Ok(())
    })
}

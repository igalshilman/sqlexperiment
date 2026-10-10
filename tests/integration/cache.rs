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

/// Another writer that checkpoints changes `meta`, which resets the owner's
/// cache; one that only appends is overridden by the log.
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

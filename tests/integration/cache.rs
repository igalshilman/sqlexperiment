use sqlite_kv::{
    ObjectCache, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation},
};
use std::sync::Arc;

fn cached(cache: &ObjectCache) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        ..StorageOptions::default()
    }
}

async fn create(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<()> {
    let mut db = Session::open_with_options(store, options).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
        .await?;
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<200) INSERT INTO t SELECT x,printf('%0900d',x) FROM n").await?;
    db.save().await?;
    Ok(())
}

async fn payload_sum(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<i64> {
    let mut db = Session::open_with_options(store, options).await?;
    let rows = db.query("SELECT sum(length(payload) + id) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn shared_cache_serves_reopened_sessions_without_object_gets() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let cache = ObjectCache::new(64 << 20);
        create(Arc::new(store.clone()), cached(&cache)).await?;
        assert!(cache.bytes() > 0, "a committed save populates the cache");

        store.reset_stats();
        let expected = 200 * 900 + 200 * 201 / 2;
        assert_eq!(
            payload_sum(Arc::new(store.clone()), cached(&cache)).await?,
            expected
        );
        let stats = store.stats();
        assert_eq!(stats.gets, 2, "only head and intent: {stats:?}");

        store.reset_stats();
        assert_eq!(
            payload_sum(Arc::new(store.clone()), StorageOptions::default()).await?,
            expected
        );
        assert!(store.stats().kind("block").gets > 0);
        Ok(())
    })
}

#[test]
fn aborted_objects_are_never_cached_when_ids_are_reused() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let cache = ObjectCache::new(64 << 20);
        create(store.clone(), cached(&cache)).await?;

        // An attempt whose head put fails allocates IDs that recovery deletes.
        let mut db = Session::open_with_options(store.clone(), cached(&cache)).await?;
        db.execute("UPDATE t SET payload = printf('%0900d', 0)")
            .await?;
        let _gate = store.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: "v10/head".into(),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        assert!(db.save().await.is_err());
        store.disarm();

        // A retry by a session without this cache reuses those IDs for different
        // pages, so a stale entry for them could never be overwritten.
        let mut db = Session::open(store.clone()).await?;
        db.execute("UPDATE t SET payload = printf('%0800d', id)")
            .await?;
        db.save().await?;

        let expected = 200 * 800 + 200 * 201 / 2;
        assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
        assert_eq!(
            payload_sum(store, StorageOptions::default()).await?,
            expected
        );
        Ok(())
    })
}

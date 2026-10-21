//! End-to-end SQL campaign: every backend mutation of a save fails in every
//! outcome, with and without a crash and an owner cache. The save is atomic, the
//! database stays consistent and the next save works.
use journal::{
    NamespaceCache, Result, Sql, StorageOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation, fresh_namespace},
};
use std::sync::Arc;

fn options(cache: Option<&NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache: cache.cloned(),
        checkpoint_after: 0,
        ..StorageOptions::default()
    }
}

/// Every save checkpoints the log.
async fn baseline(cache: Option<&NamespaceCache>) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let kv = Arc::new(store.clone());
    let mut db = Sql::open_with_options(kv.clone(), options(cache)).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, payload TEXT)")
        .await?;
    db.execute("CREATE INDEX by_n ON t(n)").await?;
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<40) INSERT INTO t SELECT x,x,printf('%0300d',x) FROM n").await?;
    db.save().await?;
    let mut db = Sql::open_with_options(kv, options(cache)).await?;
    db.execute("UPDATE t SET n = n + 100 WHERE id % 3 = 0")
        .await?;
    db.save().await?;
    Ok(store)
}

/// Returns whether the update `n = 7000` is visible; it must be all or nothing.
async fn changed(store: Arc<FaultKv>, options: StorageOptions) -> Result<bool> {
    let mut db = Sql::open_with_options(store, options).await?;
    let rows = db
        .query("SELECT count(*), sum(n = 7000), sum(length(payload)) FROM t")
        .await?;
    let (count, sevens) = (rows[0][0].as_int().unwrap(), rows[0][1].as_int().unwrap());
    assert_eq!(count, 40);
    assert_eq!(rows[0][2].as_int(), Some(40 * 300));
    assert!(sevens == 0 || sevens == 40, "torn save: {sevens} rows");
    assert_eq!(
        db.query("PRAGMA integrity_check").await?[0][0].to_string(),
        "ok"
    );
    Ok(sevens == 40)
}

#[test]
fn every_save_mutation_fails_atomically() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for cached in [false, true] {
            for operation in [Operation::Put, Operation::Delete] {
                'ordinal: for nth in 1.. {
                    for effect in [Effect::None, Effect::After] {
                        for crash in [false, true] {
                            let cache = cached.then(fresh_namespace);
                            let base = baseline(cache.as_ref()).await?;
                            let store = Arc::new(FaultKv::new(base, nth as u64));
                            let mut db =
                                Sql::open_with_options(store.clone(), options(cache.as_ref()))
                                    .await?;
                            db.execute("UPDATE t SET n = 7000").await?;
                            let _gate = store.arm(Failpoint {
                                operation,
                                key_prefix: String::new(),
                                nth,
                                effect,
                                cancel: false,
                            });
                            let result = db.save().await;
                            let faulted = store.events().iter().any(|e| e.faulted);
                            store.disarm();
                            if !faulted {
                                result?;
                                break 'ordinal;
                            }
                            cases += 1;
                            let mut cache = cache;
                            if crash {
                                store.inner.crash();
                                cache = cached.then(fresh_namespace);
                            }
                            let committed = changed(store.clone(), options(cache.as_ref())).await?;
                            if result.is_ok() {
                                assert!(committed, "acknowledged save was lost");
                            }
                            // The next save works and survives a crash.
                            let mut db =
                                Sql::open_with_options(store.clone(), options(cache.as_ref()))
                                    .await?;
                            db.execute("UPDATE t SET n = 7000").await?;
                            db.save().await?;
                            store.inner.crash();
                            assert!(changed(store.clone(), options(None)).await?);
                        }
                    }
                }
            }
        }
        assert!(cases > 40, "{cases}");
        println!("SQL save failpoints: {cases} cases");
        Ok(())
    })
}

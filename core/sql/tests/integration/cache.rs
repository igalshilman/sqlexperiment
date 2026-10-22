use crate::host::{Db, Durability};
use std::sync::Arc;
use volume::{
    NamespaceCache, Result, StorageOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv, fresh_namespace},
};

fn cached(cache: &NamespaceCache) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        ..StorageOptions::default()
    }
}

async fn create(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<()> {
    let mut db = Db::open(store, options).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
        .await?;
    // Incompressible: a large save that checkpoints itself through the cache.
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<400) INSERT INTO t SELECT x,randomblob(900) FROM n").await?;
    db.save().await?;
    Ok(())
}

async fn payload_sum(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<i64> {
    let mut db = Db::open(store, options).await?;
    let rows = db.query("SELECT sum(length(payload) + id) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn an_owner_cache_serves_reopened_sessions_in_one_round_trip() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        create(store.clone(), cached(&cache)).await?;
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
        let cache = fresh_namespace();
        create(store.clone(), cached(&cache)).await?;
        let mut expected = 400 * 900 + 400 * 401 / 2;
        assert_eq!(payload_sum(store.clone(), cached(&cache)).await?, expected);
        for (round, checkpoint_after) in [(0, 0), (1, 100), (2, 100), (3, 0)] {
            let other = StorageOptions {
                checkpoint_after,
                ..StorageOptions::default()
            };
            let mut db = Db::open(store.clone(), other).await?;
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

fn owner(cache: &NamespaceCache) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        ..StorageOptions::default()
    }
}
async fn count(db: &mut Db) -> Result<i64> {
    Ok(db.query("SELECT count(*) FROM t").await?[0][0]
        .as_int()
        .unwrap())
}

/// A clean save or read-only session leaves its engine to the next session; a
/// session with unsaved changes, an error, or a write by anyone else does not.
#[test]
fn warm_engines_continue_only_from_an_unchanged_committed_state() -> Result<()> {
    use volume::simulation::{Effect, Failpoint, FaultKv, Operation};
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let cache = fresh_namespace();
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine());
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").await?;
        db.save().await?;

        // After a save, and after a read-only session, the engine continues.
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine(), "after a save");
        db.execute("INSERT INTO t VALUES (1)").await?;
        db.save().await?;
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine());
        assert_eq!(count(&mut db).await?, 1);
        drop(db);
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine(), "after a read-only session");

        // Unsaved changes discard the engine with them.
        db.execute("INSERT INTO t VALUES (2)").await?;
        drop(db);
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine(), "unsaved changes");
        assert_eq!(count(&mut db).await?, 1);
        drop(db);

        // An SQL error leaves the session, and so its engine, usable.
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine());
        assert!(db.execute("INSERT INTO missing VALUES (1)").await.is_err());
        assert!(db.is_usable());
        drop(db);
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(db.reused_engine(), "after an SQL error");
        assert_eq!(count(&mut db).await?, 1);
        drop(db);

        // Another writer appends, then checkpoints: both force a fresh engine.
        for checkpoint_after in [100, 0] {
            let other = StorageOptions {
                checkpoint_after,
                ..StorageOptions::default()
            };
            let mut db = Db::open(store.clone(), other).await?;
            db.execute("INSERT INTO t SELECT max(id) + 1 FROM t")
                .await?;
            db.save().await?;
            let mut db = Db::open(store.clone(), owner(&cache)).await?;
            assert!(!db.reused_engine(), "another writer");
            let expected = if checkpoint_after == 0 { 3 } else { 2 };
            assert_eq!(count(&mut db).await?, expected);
            drop(db);
            let db = Db::open(store.clone(), owner(&cache)).await?;
            assert!(db.reused_engine(), "warm again once unchanged");
        }

        // A save that fails before committing leaves nothing to reuse.
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        db.execute("INSERT INTO t VALUES (100)").await?;
        let _gate = store.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: "v15/commit".into(),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        assert!(db.save().await.is_err());
        store.disarm();
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine(), "failed save");
        assert_eq!(count(&mut db).await?, 3);
        Ok(())
    })
}

/// A brief other owner that does not checkpoint leaves this owner's cache
/// warm: the log it appended overrides the cached pages. Once it checkpoints
/// (and deletes the log), the cache drops its pages.
#[test]
fn a_brief_other_owner_leaves_the_cache_warm() -> Result<()> {
    futures::executor::block_on(async {
        for (saves, warm) in [(3, true), (6, false)] {
            let store = Arc::new(LatencyKv::new(MemoryKv::default()));
            let cache = fresh_namespace();
            create(store.clone(), cached(&cache)).await?;
            let other = fresh_namespace();
            for n in 0..saves {
                // Its fifth save checkpoints.
                let options = StorageOptions {
                    checkpoint_after: 4,
                    ..cached(&other)
                };
                let mut db = Db::open(store.clone(), options).await?;
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

/// Namespaces with byte-identical `meta` share one process cache without ever
/// reading each other's pages, even with sessions running concurrently.
#[test]
fn namespaces_share_one_cache_without_sharing_pages() -> Result<()> {
    futures::executor::block_on(async {
        let cache = volume::simulation::shared_cache();
        let stores: Vec<Arc<dyn KvStore>> = (0..2)
            .map(|_| Arc::new(MemoryKv::default()) as Arc<dyn KvStore>)
            .collect();
        let options = |n: usize| StorageOptions {
            cache: Some(cache.namespace(&format!("shared-{n}"))),
            checkpoint_after: 0,
            ..StorageOptions::default()
        };
        // The same history shape, so the same meta, with different values.
        for (n, store) in stores.iter().enumerate() {
            let mut db = Db::open(store.clone(), options(n)).await?;
            db.execute("CREATE TABLE t(v TEXT)").await?;
            db.save().await?;
            let mut db = Db::open(store.clone(), options(n)).await?;
            db.execute(&format!("INSERT INTO t VALUES ('from {n}')"))
                .await?;
            db.save().await?;
        }
        let read = |n: usize| {
            let store = stores[n].clone();
            async move {
                let mut db = Db::open(store, options(n)).await?;
                let rows = db.query("SELECT v FROM t").await?;
                Result::Ok(rows[0][0].to_string())
            }
        };
        for _ in 0..3 {
            let (a, b) = futures::try_join!(read(0), read(1))?;
            assert_eq!((a.as_str(), b.as_str()), ("from 0", "from 1"));
        }
        Ok(())
    })
}

/// Pages evicted from memory are served from disk.
#[test]
fn evicted_pages_are_read_from_disk() -> Result<()> {
    use volume::simulation::{cache_config, open_cache};
    futures::executor::block_on(async {
        let cache = open_cache(cache_config(64 << 10, 64 << 20));
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let namespace = cache.namespace("spilled");
        create(store.clone(), cached(&namespace)).await?;
        assert!(cache.memory_bytes() <= 64 << 10);
        store.inner.reset_stats();
        let expected = 400 * 900 + 400 * 401 / 2;
        assert_eq!(
            payload_sum(store.clone(), cached(&namespace)).await?,
            expected
        );
        assert_eq!(store.inner.stats().kind("page").gets, 0);
        Ok(())
    })
}

/// Parked engines are bounded by memory across namespaces.
#[test]
fn parked_engines_are_bounded() -> Result<()> {
    use volume::{
        PageCacheConfig,
        simulation::{cache_config, open_cache},
    };
    futures::executor::block_on(async {
        let cache = open_cache(PageCacheConfig {
            idle_bytes: (4 << 20) + (12 << 10), // one engine with an empty schema
            ..cache_config(1 << 20, 16 << 20)
        });
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let other: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let a = || owner(&cache.namespace("a"));
        drop(Db::open(store.clone(), a()).await?);
        assert!(Db::open(store.clone(), a()).await?.reused_engine());
        drop(Db::open(other, owner(&cache.namespace("b"))).await?);
        let db = Db::open(store, a()).await?;
        assert!(!db.reused_engine(), "evicted by b's engine");
        Ok(())
    })
}

/// A read-only session that overlaps a save must not park its older engine
/// under the newer commit: the next session would read and save stale state.
#[test]
fn an_overlapping_session_never_parks_a_stale_engine() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").await?;
        db.save().await?;

        let mut reader = Db::open(store.clone(), owner(&cache)).await?;
        assert_eq!(count(&mut reader).await?, 0);
        let mut writer = Db::open(store.clone(), owner(&cache)).await?;
        writer.execute("INSERT INTO t VALUES (1)").await?;
        writer.save().await?;
        drop(reader);

        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert_eq!(count(&mut db).await?, 1);
        db.execute("INSERT INTO t VALUES (2)").await?;
        db.save().await?;
        let mut cold = Db::open(store, StorageOptions::default()).await?;
        assert_eq!(count(&mut cold).await?, 2, "no commit lost");
        Ok(())
    })
}

/// Settings, attachments and the temp schema belong to a connection; a session
/// that changes them does not hand its engine to the next one.
#[test]
fn connection_state_never_reaches_the_next_session() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").await?;
        db.save().await?;

        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        db.execute("PRAGMA query_only = 1").await?;
        drop(db);
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine());
        db.execute("INSERT INTO t VALUES (1)").await?;
        db.execute("CREATE TEMP TABLE scratch(x)").await?;
        db.save().await?;

        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert!(!db.reused_engine());
        assert!(db.query("SELECT * FROM scratch").await.is_err());
        drop(db);
        // Ordinary SQL keeps the engine warm.
        let mut db = Db::open(store.clone(), owner(&cache)).await?;
        assert_eq!(count(&mut db).await?, 1);
        drop(db);
        let db = Db::open(store, owner(&cache)).await?;
        assert!(db.reused_engine());
        Ok(())
    })
}

/// A reused engine starts with fresh connection counters, exactly like a cold
/// one, so the same SQL gives the same answer either way.
#[test]
fn a_reused_engine_has_fresh_connection_counters() -> volume::Result<()> {
    futures::executor::block_on(async {
        let store = std::sync::Arc::new(volume::simulation::MemoryKv::default());
        let options = volume::StorageOptions {
            cache: Some(volume::simulation::fresh_namespace()),
            ..volume::StorageOptions::default()
        };
        let mut db = Db::open(store.clone(), options.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)").await?;
        db.execute("INSERT INTO t VALUES(42)").await?;
        db.save().await?;
        let counters = "SELECT last_insert_rowid(), changes(), total_changes()";
        let mut warm = Db::open(store.clone(), options).await?;
        assert!(warm.reused_engine());
        let mut cold = Db::open(store, StorageOptions::default()).await?;
        assert_eq!(warm.query(counters).await?, cold.query(counters).await?);
        assert_eq!(warm.query(counters).await?[0][0].as_int(), Some(0));
        Ok(())
    })
}

/// Evicting a namespace's state from the LRU while a session still holds its
/// handle must not give the next session a second, independent state: the two
/// would not see each other's commits and could both write one log position.
#[test]
fn an_evicted_namespace_keeps_one_state_while_in_use() -> Result<()> {
    use volume::simulation::{cache_config, open_cache};
    futures::executor::block_on(async {
        let cache = open_cache(volume::PageCacheConfig {
            namespaces: 1,
            ..cache_config(1 << 20, 64 << 20)
        });
        let store = Arc::new(MemoryKv::default());
        let options = |namespace: NamespaceCache| {
            (
                StorageOptions {
                    cache: Some(namespace),
                    ..StorageOptions::default()
                },
                Durability::Commit,
            )
        };
        let mut first = Db::open(store.clone(), options(cache.namespace("a"))).await?;
        first
            .execute("CREATE TABLE t(id INTEGER PRIMARY KEY)")
            .await?;
        for other in 0..1000 {
            cache.namespace(&format!("other-{other}")); // evicts "a"
        }
        let mut second = Db::open(store.clone(), options(cache.namespace("a"))).await?;
        second.execute("INSERT INTO t VALUES(1)").await?;
        assert!(first.execute("INSERT INTO t VALUES(2)").await.is_err());
        drop((first, second));
        let mut db = Db::open(store, StorageOptions::default()).await?;
        assert_eq!(db.query("SELECT id FROM t").await?.len(), 1);
        Ok(())
    })
}

/// A parked engine is weighed by its schema: one with a large schema takes the
/// room of several small ones.
#[test]
fn parked_engines_are_weighed_by_schema() -> Result<()> {
    use volume::{
        PageCacheConfig,
        simulation::{cache_config, open_cache},
    };
    futures::executor::block_on(async {
        let cache = open_cache(PageCacheConfig {
            idle_bytes: (16 << 20) + (100 << 10), // four engines with empty schemas
            ..cache_config(1 << 20, 16 << 20)
        });
        let open = |name: &str, store: &Arc<dyn KvStore>| {
            Db::open(store.clone(), owner(&cache.namespace(name)))
        };
        let small: Vec<Arc<dyn KvStore>> = (0..4)
            .map(|_| Arc::new(MemoryKv::default()) as Arc<dyn KvStore>)
            .collect();
        for (i, store) in small.iter().enumerate() {
            drop(open(&format!("small-{i}"), store).await?);
        }
        for (i, store) in small.iter().enumerate() {
            let db = open(&format!("small-{i}"), store).await?;
            assert!(db.reused_engine(), "four small engines fit");
        }

        let big: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let mut db = open("big", &big).await?;
        for t in 0..200 {
            db.execute(&format!(
                "CREATE TABLE t{t}(id INTEGER PRIMARY KEY, a TEXT, b TEXT); CREATE INDEX i{t} ON t{t}(a, b)"
            ))
            .await?;
        }
        db.save().await?; // parks, weighed by its schema
        let mut kept = 0;
        for (i, store) in small.iter().enumerate() {
            kept += usize::from(open(&format!("small-{i}"), store).await?.reused_engine());
        }
        assert!(kept < 4, "the big engine displaced small ones: {kept} kept");
        Ok(())
    })
}

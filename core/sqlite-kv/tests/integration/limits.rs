//! Memory limits: private changes, result rows and the replayed log.
use bytes::Bytes;
use futures::future::BoxFuture;
use sqlite_kv::{Durability, Result, Session, StorageOptions, kv::KvStore, simulation::MemoryKv};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[test]
fn private_changes_are_bounded() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = StorageOptions {
            max_private_bytes: 1 << 20,
            ..StorageOptions::default()
        };
        let mut db = Session::open_with_options(store.clone(), options.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        db.execute("INSERT INTO t VALUES(1, randomblob(100000))")
            .await?;
        db.save().await?;
        let mut db = Session::open_with_options(store.clone(), options.clone()).await?;
        let error = db
            .execute("INSERT INTO t SELECT value, randomblob(100000) FROM (WITH RECURSIVE n(value) AS (VALUES(2) UNION ALL SELECT value + 1 FROM n WHERE value < 40) SELECT value FROM n)")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("max_private_bytes"), "{error}");
        assert!(db.execute("SELECT 1").await.is_err(), "poisoned");
        drop(db);
        let mut db = Session::open_with_options(store, options).await?;
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(1)
        );
        Ok(())
    })
}

#[test]
fn result_rows_are_bounded() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = StorageOptions {
            max_result_bytes: 64 << 10,
            ..StorageOptions::default()
        };
        let mut db = Session::open_with_options(store, options).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        for i in 0..20 {
            db.execute(&format!("INSERT INTO t VALUES({i}, zeroblob(10000))"))
                .await?;
        }
        assert_eq!(db.query("SELECT id FROM t").await?.len(), 20);
        let error = db.query("SELECT body FROM t").await.unwrap_err();
        assert!(error.to_string().contains("max_result_bytes"), "{error}");
        Ok(())
    })
}

/// Fails every page put while `failing` is set.
struct FailingPages {
    inner: MemoryKv,
    failing: AtomicBool,
}

impl KvStore for FailingPages {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        self.inner.get(key)
    }
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        if key.contains("/page/") && self.failing.load(Ordering::SeqCst) {
            return Box::pin(async { Err(io::Error::other("injected page failure")) });
        }
        self.inner.put(key, value)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.inner.delete(key)
    }
}

/// A log grown by a long run of failed checkpoints shrinks again once they
/// succeed, so replay at open stays bounded.
#[test]
fn a_grown_log_catches_up() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FailingPages {
            inner: MemoryKv::default(),
            failing: AtomicBool::new(true),
        });
        let options = StorageOptions {
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let log_bytes = |store: &FailingPages| {
            store
                .inner
                .keys()
                .iter()
                .filter(|key| key.contains("/log/"))
                .count()
        };
        let mut db = Session::open_with_options(store.clone(), options.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        for i in 0..120 {
            db.execute(&format!("INSERT INTO t VALUES({i}, randomblob(50000))"))
                .await?;
        }
        assert!(db.take_cleanup_error().is_some());
        let grown = log_bytes(&store);
        assert!(grown > 100, "{grown} segments");
        store.failing.store(false, Ordering::SeqCst);
        for i in 0..10 {
            db.execute(&format!(
                "INSERT INTO t VALUES({}, randomblob(100))",
                1000 + i
            ))
            .await?;
        }
        // Back under the catch-up threshold (4 MiB, about 80 of these segments).
        let after = log_bytes(&store);
        assert!(after < 80, "{grown} segments, then {after}");
        drop(db);
        let mut db = Session::open_with_options(store, options).await?;
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(130)
        );
        Ok(())
    })
}

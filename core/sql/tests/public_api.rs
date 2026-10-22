//! Exercises the normal library with an immediately-ready backend: no simulator,
//! instrumentation, forced yields or checkpoint fault hooks are required.
use bytes::Bytes;
use futures::future::BoxFuture;
use sql::Sql;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};
use volume::{Result, Volume, kv::KvStore};

/// Flush and commit `db`'s volume, ending the session.
async fn save(mut db: Sql) -> volume::Result<volume::CommitReport> {
    db.flush().await?;
    db.volume().commit().await
}

#[derive(Default)]
struct Store(Mutex<BTreeMap<String, Bytes>>);

impl KvStore for Store {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().get(key).cloned()) })
    }
    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.0.lock().unwrap().insert(key.into(), value);
        Ok(())
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn save_and_reopen_with_an_immediately_ready_backend() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Store::default());
        fn require_send<T: Send>(value: T) -> T {
            value
        }
        let mut db = require_send(Sql::open(&Volume::open(store.clone()).await?)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        db.execute("INSERT INTO t VALUES (1, 'hello')").await?;
        save(db).await?;
        let mut db = Sql::open(&Volume::open(store.clone()).await?).await?;
        assert_eq!(
            db.query("SELECT value FROM t").await?[0][0].to_string(),
            "hello"
        );
        db.execute("UPDATE t SET value='changed'").await?;
        drop(db);
        let mut db = Sql::open(&Volume::open(store).await?).await?;
        assert_eq!(
            db.query("SELECT value FROM t").await?[0][0].to_string(),
            "hello"
        );
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        Ok(())
    })
}

/// A transaction and records, durable together at the volume's commit.
#[test]
fn a_volume_commit_with_an_immediately_ready_backend() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Store::default());
        fn require_send<T: Send>(value: T) -> T {
            value
        }
        let volume = Volume::open(store.clone()).await?;
        let mut db = Sql::open(&volume).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        require_send(db.transaction(async |db| {
            db.execute("INSERT INTO t VALUES (1, 'hello')").await?;
            db.execute("INSERT INTO t VALUES (2, 'world')").await
        }))
        .await?;
        db.flush().await?;
        volume.record("note").set(Bytes::from_static(b"two rows"));
        require_send(volume.commit()).await?;
        drop(db);
        let volume = Volume::open(store).await?;
        assert_eq!(volume.record("note").get(), &b"two rows"[..]);
        let mut db = Sql::open(&volume).await?;
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(2)
        );
        Ok(())
    })
}

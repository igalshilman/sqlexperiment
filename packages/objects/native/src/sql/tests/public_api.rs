//! Exercises the normal library with an immediately-ready backend: no simulator,
//! instrumentation, forced yields or checkpoint fault hooks are required.
use crate::sql::Database;
use crate::state::{Result, kv::KvStore};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

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
        let db = require_send(Database::open(store.clone())).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        db.execute("INSERT INTO t VALUES (1, 'hello')").await?;
        require_send(db.save()).await?;
        let db = Database::open(store.clone()).await?;
        assert_eq!(
            db.query("SELECT value FROM t").await?[0][0].to_string(),
            "hello"
        );
        db.begin().await?;
        db.execute("UPDATE t SET value='changed'").await?;
        drop(db);
        let db = Database::open(store).await?;
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

/// A transaction and its effects, durable together at its commit.
#[test]
fn a_commit_with_effects_with_an_immediately_ready_backend() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Store::default());
        fn require_send<T: Send>(value: T) -> T {
            value
        }
        let db = Database::open(store.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        db.execute("INSERT INTO t VALUES (1, 'hello')").await?;
        require_send(db.begin()).await?;
        db.execute("INSERT INTO t VALUES (2, 'world')").await?;
        require_send(db.commit(Some(Bytes::from_static(b"two rows")))).await?;
        drop(db);
        let db = Database::open(store).await?;
        assert_eq!(db.effects().await?, &b"two rows"[..]);
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(2)
        );
        Ok(())
    })
}

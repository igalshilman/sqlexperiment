//! Exercises the normal library with an immediately-ready backend: no simulator,
//! instrumentation, forced yields or checkpoint fault hooks are required.
use futures::future::BoxFuture;
use sqlite_kv::{Result, Session, kv::KvStore};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Store(Mutex<BTreeMap<String, Vec<u8>>>);

impl KvStore for Store {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().get(key).cloned()) })
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().insert(key.into(), value);
            Ok(())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().remove(key);
            Ok(())
        })
    }
}

#[test]
fn save_and_reopen_with_an_immediately_ready_backend() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Store::default());
        let mut db = Session::open(store.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        db.execute("INSERT INTO t VALUES (1, 'hello')").await?;
        assert!(db.save().await?.cleanup_error.is_none());
        let mut db = Session::open(store.clone()).await?;
        assert_eq!(
            db.query("SELECT value FROM t").await?[0][0].to_string(),
            "hello"
        );
        db.execute("UPDATE t SET value='changed'").await?;
        drop(db);
        let mut db = Session::open(store).await?;
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

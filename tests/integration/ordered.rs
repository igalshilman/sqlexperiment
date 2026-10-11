//! A store that acknowledges writes as it orders them (as Restate state writes
//! are): writes are stored in order, and a failed request loses its unconfirmed
//! tail while the process, and its owner cache, live on. The owner cache then
//! describes writes that do not exist, so the host must reset it before the next
//! open; without that, a later save records a checkpoint whose pages were lost.
use bytes::Bytes;
use futures::future::BoxFuture;
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions, Value, kv::KvStore,
    simulation::fresh_namespace,
};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

/// Stored values, and writes acknowledged but not yet confirmed.
#[derive(Default)]
struct Ordered {
    stored: Mutex<HashMap<String, Bytes>>,
    unconfirmed: Mutex<Vec<(String, Option<Bytes>)>>,
}

impl Ordered {
    /// Every write so far is stored.
    fn confirm(&self) {
        let mut stored = self.stored.lock().unwrap();
        for (key, value) in self.unconfirmed.lock().unwrap().drain(..) {
            match value {
                Some(value) => stored.insert(key, value),
                None => stored.remove(&key),
            };
        }
    }

    /// The request failed: its unconfirmed writes are lost.
    fn lose(&self) {
        self.unconfirmed.lock().unwrap().clear();
    }
}

/// A request's view: its own writes over the stored values.
struct Request(Arc<Ordered>);

impl KvStore for Request {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        let unconfirmed = self.0.unconfirmed.lock().unwrap();
        let value = match unconfirmed.iter().rev().find(|(k, _)| k == key) {
            Some((_, value)) => value.clone(),
            None => self.0.stored.lock().unwrap().get(key).cloned(),
        };
        Box::pin(async move { Ok(value) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        let write = (key.to_owned(), Some(value));
        self.0.unconfirmed.lock().unwrap().push(write);
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.0
            .unconfirmed
            .lock()
            .unwrap()
            .push((key.to_owned(), None));
        Box::pin(async { Ok(()) })
    }

    fn durable_when_visible(&self) -> bool {
        true
    }
}

async fn open(store: &Arc<Ordered>, cache: Option<&NamespaceCache>) -> Result<Session> {
    let options = StorageOptions {
        cache: cache.cloned(),
        durability: Durability::Commit,
        ..StorageOptions::default()
    };
    Session::open_with_options(Arc::new(Request(store.clone())), options).await
}

/// Rows and integrity as a process with no cache reads them.
async fn fresh_view(store: &Arc<Ordered>) -> Result<(i64, String)> {
    let mut db = open(store, None).await?;
    let rows = db.query("SELECT count(*) FROM t").await?[0][0]
        .as_int()
        .unwrap();
    let integrity = db.query("PRAGMA integrity_check").await?[0][0].to_string();
    Ok((rows, integrity))
}

/// Requests insert a row each; request `lost` loses its writes. Returns whether
/// a process without the cache then reads exactly the confirmed rows.
async fn consistent_after_loss(lost: i64, reset: bool) -> Result<bool> {
    let store = Arc::new(Ordered::default());
    let cache = fresh_namespace();
    open(&store, Some(&cache))
        .await?
        .execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v BLOB)")
        .await?;
    store.confirm();
    let mut expected = 0;
    for id in 0..20 {
        let insert = "INSERT INTO t VALUES(?, randomblob(3000))";
        open(&store, Some(&cache))
            .await?
            .execute_with(insert, [Value::from_i64(id)])
            .await?;
        if id == lost {
            store.lose();
            if reset {
                cache.reset();
            }
        } else {
            store.confirm();
            expected += 1;
        }
    }
    Ok(match fresh_view(&store).await {
        Ok((rows, integrity)) => rows == expected && integrity == "ok",
        Err(_) => false,
    })
}

#[tokio::test]
async fn reset_after_lost_writes_keeps_the_store_consistent() -> Result<()> {
    for lost in 1..15 {
        assert!(
            consistent_after_loss(lost, true).await?,
            "lost request {lost}"
        );
    }
    Ok(())
}

/// The hazard the reset prevents: a lost checkpoint the owner still claims.
#[tokio::test]
async fn trusting_lost_writes_corrupts_the_store() -> Result<()> {
    let mut corrupt = 0;
    for lost in 1..15 {
        corrupt += usize::from(!consistent_after_loss(lost, false).await?);
    }
    assert!(corrupt > 0, "no loss reached a checkpoint");
    Ok(())
}

//! An ordered store (as Restate state writes are): writes are acknowledged once
//! ordered, stored in order, and a failed request loses its unconfirmed tail
//! while the process, and its owner cache, live on. A session confirms its
//! writes in `save`, and one that did not makes the next start cold. A save
//! records a checkpoint only after writing its pages itself, so even a store
//! that hid its ordering loses whole requests only.
use bytes::Bytes;
use futures::future::BoxFuture;
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions, Value, kv::KvStore,
    simulation::fresh_namespace,
};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Stored values, and writes acknowledged but not yet confirmed.
#[derive(Default)]
struct Backend {
    stored: Mutex<HashMap<String, Bytes>>,
    unconfirmed: Mutex<Vec<(String, Option<Bytes>)>>,
}

impl Backend {
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
struct Request {
    backend: Arc<Backend>,
    /// Whether the store admits it is ordered.
    ordered: bool,
    /// This request fails before its writes are stored.
    fails: AtomicBool,
}

impl KvStore for Request {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        let unconfirmed = self.backend.unconfirmed.lock().unwrap();
        let value = match unconfirmed.iter().rev().find(|(k, _)| k == key) {
            Some((_, value)) => value.clone(),
            None => self.backend.stored.lock().unwrap().get(key).cloned(),
        };
        Box::pin(async move { Ok(value) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        let write = (key.to_owned(), Some(value));
        self.backend.unconfirmed.lock().unwrap().push(write);
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        let write = (key.to_owned(), None);
        self.backend.unconfirmed.lock().unwrap().push(write);
        Box::pin(async { Ok(()) })
    }

    fn durable_when_visible(&self) -> bool {
        true
    }

    fn ordered(&self) -> bool {
        self.ordered
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async {
            if self.fails.load(Ordering::SeqCst) {
                self.backend.lose();
                return Err(io::Error::other("the request failed"));
            }
            self.backend.confirm();
            Ok(())
        })
    }
}

fn request(backend: &Arc<Backend>, ordered: bool, fails: bool) -> Arc<Request> {
    Arc::new(Request {
        backend: backend.clone(),
        ordered,
        fails: AtomicBool::new(fails),
    })
}

async fn open(store: Arc<Request>, cache: Option<&NamespaceCache>) -> Result<Session> {
    let options = StorageOptions {
        cache: cache.cloned(),
        durability: Durability::Commit,
        ..StorageOptions::default()
    };
    Session::open_with_options(store, options).await
}

/// How a failed request ends.
#[derive(Clone, Copy, Debug)]
enum Failure {
    /// Its save fails to confirm the writes.
    Confirm,
    /// It is dropped before saving, as a cancelled handler.
    Dropped,
}

/// Requests insert a row each and save; request `lost` fails and loses its
/// writes. Returns whether a process without the cache then reads exactly the
/// stored rows, as does the owner.
async fn consistent_after_loss(lost: i64, ordered: bool, failure: Failure) -> Result<bool> {
    let backend = Arc::new(Backend::default());
    let cache = fresh_namespace();
    let mut db = open(request(&backend, ordered, false), Some(&cache)).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v BLOB)")
        .await?;
    db.save().await?;
    backend.confirm(); // the request succeeded
    let mut expected = 0;
    for id in 0..20 {
        let fails = id == lost;
        let mut db = open(request(&backend, ordered, fails), Some(&cache)).await?;
        let insert = "INSERT INTO t VALUES(?, randomblob(3000))";
        db.execute_with(insert, [Value::from_i64(id)]).await?;
        match (fails, failure) {
            (false, _) => {
                db.save().await?;
                backend.confirm(); // the request succeeded
                expected += 1;
            }
            (true, Failure::Confirm) => assert!(db.save().await.is_err() || !ordered),
            (true, Failure::Dropped) => drop(db),
        }
        if fails {
            backend.lose();
        }
    }
    for cache in [None, Some(&cache)] {
        let view = async {
            let mut db = open(request(&backend, ordered, false), cache).await?;
            let rows = db.query("SELECT count(*) FROM t").await?[0][0]
                .as_int()
                .unwrap();
            let integrity = db.query("PRAGMA integrity_check").await?[0][0].to_string();
            Result::Ok((rows, integrity))
        };
        match view.await {
            Ok((rows, integrity)) if rows == expected && integrity == "ok" => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

#[tokio::test]
async fn an_ordered_store_keeps_every_loss_consistent() -> Result<()> {
    for failure in [Failure::Confirm, Failure::Dropped] {
        for lost in 1..15 {
            let consistent = consistent_after_loss(lost, true, failure).await?;
            assert!(consistent, "lost request {lost}, {failure:?}");
        }
    }
    Ok(())
}

/// A store that acknowledges before storing but does not say so: no save
/// records a checkpoint whose pages an earlier, lost request wrote.
#[tokio::test]
async fn a_hidden_ordered_store_loses_whole_requests_only() -> Result<()> {
    for lost in 1..15 {
        let consistent = consistent_after_loss(lost, false, Failure::Confirm).await?;
        assert!(consistent, "lost request {lost}");
    }
    Ok(())
}

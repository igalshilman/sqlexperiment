//! Races between sessions of one owner that share its cache.
use bytes::Bytes;
use futures::{
    channel::oneshot,
    future::{BoxFuture, FutureExt, Shared},
};
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{MemoryKv, fresh_namespace},
};
use std::{
    io,
    sync::{Arc, Mutex},
};

fn opts(cache: &NamespaceCache, durability: Durability) -> StorageOptions {
    StorageOptions {
        cache: Some(cache.clone()),
        durability,
        hint_pages: 0,
        ..StorageOptions::default()
    }
}

/// Reads the value of a page key, then holds it until released.
struct GateKv {
    inner: MemoryKv,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<Shared<oneshot::Receiver<()>>>>,
}
impl KvStore for GateKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            let value = self.inner.get(key).await?;
            let gate = match key.starts_with("v12/page/") {
                true => self.release.lock().unwrap().clone(),
                false => None,
            };
            if let Some(gate) = gate {
                if let Some(tx) = self.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = gate.await;
            }
            Ok(value)
        })
    }
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        self.inner.put(key, value)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.inner.delete(key)
    }
}

async fn value(db: &mut Session, id: i64) -> Result<String> {
    Ok(db
        .query(&format!("SELECT v FROM t WHERE id = {id}"))
        .await?[0][0]
        .to_string())
}

/// A page get issued before another session's checkpoint replaced that page in
/// the cache must not land under the new version: the cache would serve the
/// old image to every later session, which would then save on top of it.
#[test]
fn an_overlapping_read_never_caches_a_replaced_page() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let gate = Arc::new(GateKv {
            inner: memory.clone(),
            started: Mutex::new(None),
            release: Mutex::new(None),
        });
        let plain: Arc<dyn KvStore> = Arc::new(memory.clone());
        // Setup without a cache: a large save checkpoints every page in place.
        let mut db = Session::open(plain.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")
            .await?;
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<600) INSERT INTO t SELECT x, 'old' || x || '|' || hex(randomblob(700)) FROM n").await?;
        db.save().await?;

        let ns = fresh_namespace();
        let mut reader =
            Session::open_with_options(gate.clone(), opts(&ns, Durability::Save)).await?;
        assert!(value(&mut reader, 1).await?.starts_with("old1"));
        let (tx, started) = oneshot::channel();
        *gate.started.lock().unwrap() = Some(tx);
        let (release_tx, release_rx) = oneshot::channel::<()>();
        *gate.release.lock().unwrap() = Some(release_rx.shared());

        let mut writer = Session::open_with_options(
            plain.clone(),
            StorageOptions {
                checkpoint_after: 0,
                ..opts(&ns, Durability::Commit)
            },
        )
        .await?;
        let read = value(&mut reader, 250);
        let write = async {
            started.await.unwrap();
            eprintln!("writer starts");
            writer
                .execute("UPDATE t SET v = 'new250' WHERE id = 250")
                .await?;
            // Checkpoints the previous commit: page/<leaf of 250> and the cache.
            writer
                .execute("UPDATE t SET v = 'new1' WHERE id = 1")
                .await?;
            assert_eq!(value(&mut writer, 250).await?, "new250");
            writer.execute("PRAGMA user_version").await?; // no engine reuse
            release_tx.send(()).unwrap();
            Result::Ok(())
        };
        let (read, write) = futures::join!(read, write);
        write?;
        read?;
        drop(reader);
        drop(writer);
        *gate.release.lock().unwrap() = None;

        let mut db =
            Session::open_with_options(plain.clone(), opts(&ns, Durability::Commit)).await?;
        assert!(!db.reused_engine());
        let seen = value(&mut db, 250).await?;
        let seen = seen[..seen.len().min(6)].to_owned();
        // A neighbour on the same leaf, committed on top of what the cache served.
        db.execute("UPDATE t SET v = substr(v, 1, 6) || '!' WHERE id = 250")
            .await?;
        drop(db);
        let mut cold = Session::open(plain).await?;
        let durable = value(&mut cold, 250).await?;
        assert_eq!(
            (seen.as_str(), durable.as_str()),
            ("new250", "new250!"),
            "cached / durable value of an acknowledged commit"
        );
        Ok(())
    })
}

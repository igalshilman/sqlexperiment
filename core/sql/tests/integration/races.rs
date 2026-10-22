//! Races between sessions of one owner that share its cache.
use crate::host::{Db, Durability, Setup};
use bytes::Bytes;
use futures::{
    channel::oneshot,
    future::{BoxFuture, FutureExt, Shared},
};
use std::{
    io,
    sync::{Arc, Mutex},
};
use volume::{
    NamespaceCache, Result, StorageOptions,
    kv::KvStore,
    simulation::{MemoryKv, fresh_namespace},
};

fn opts(cache: &NamespaceCache, durability: Durability) -> Setup {
    Setup(
        StorageOptions {
            cache: Some(cache.clone()),
            hint_pages: 0,
            ..StorageOptions::default()
        },
        durability,
    )
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
            let gate = match key.starts_with("v15/page/") {
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
    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.inner.put(key, value)
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.inner.delete(key)
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        self.inner.sync()
    }
}

async fn value(db: &mut Db, id: i64) -> Result<String> {
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
        let mut db = Db::open(plain.clone(), StorageOptions::default()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")
            .await?;
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<600) INSERT INTO t SELECT x, 'old' || x || '|' || hex(randomblob(700)) FROM n").await?;
        db.save().await?;

        let ns = fresh_namespace();
        let mut reader = Db::open(gate.clone(), opts(&ns, Durability::Save)).await?;
        assert!(value(&mut reader, 1).await?.starts_with("old1"));
        let (tx, started) = oneshot::channel();
        *gate.started.lock().unwrap() = Some(tx);
        let (release_tx, release_rx) = oneshot::channel::<()>();
        *gate.release.lock().unwrap() = Some(release_rx.shared());

        let mut writer = Db::open(
            plain.clone(),
            opts(&ns, Durability::Commit).with(|o| StorageOptions {
                checkpoint_after: 0,
                ..o
            }),
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

        let mut db = Db::open(plain.clone(), opts(&ns, Durability::Commit)).await?;
        assert!(!db.reused_engine());
        let seen = value(&mut db, 250).await?;
        let seen = seen[..seen.len().min(6)].to_owned();
        // A neighbour on the same leaf, committed on top of what the cache served.
        db.execute("UPDATE t SET v = substr(v, 1, 6) || '!' WHERE id = 250")
            .await?;
        drop(db);
        let mut cold = Db::open(plain, StorageOptions::default()).await?;
        let durable = value(&mut cold, 250).await?;
        assert_eq!(
            (seen.as_str(), durable.as_str()),
            ("new250", "new250!"),
            "cached / durable value of an acknowledged commit"
        );
        Ok(())
    })
}

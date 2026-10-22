use crate::host::Db;
use bytes::Bytes;
use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either, select},
};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use volume::{Result, StorageOptions, kv::KvStore, simulation::MemoryKv};

struct ControlledKv {
    inner: MemoryKv,
    fail_reads: AtomicBool,
    gate: Mutex<Option<oneshot::Sender<()>>>,
}

impl KvStore for ControlledKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            if key.contains("/page/") {
                if self.fail_reads.load(Ordering::SeqCst) {
                    return Err(io::Error::other("injected page-read failure"));
                }
                let signal = self.gate.lock().unwrap().take();
                if let Some(signal) = signal {
                    let _ = signal.send(());
                    futures::future::pending::<()>().await;
                }
            }
            self.inner.get(key).await
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

async fn populated() -> Result<Arc<ControlledKv>> {
    let store = Arc::new(ControlledKv {
        inner: MemoryKv::default(),
        fail_reads: AtomicBool::new(false),
        gate: Mutex::new(None),
    });
    let mut db = Db::open(store.clone(), StorageOptions::default()).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
        .await?;
    db.execute("INSERT INTO t VALUES(1,10)").await?;
    db.checkpoint().await?;
    db.save().await?;
    // A save that checkpoints the log, so reading `t` goes to the backend,
    // then one that leaves the schema page in the log, for open to read.
    let all = StorageOptions {
        checkpoint_after: 0,
        ..StorageOptions::default()
    };
    let mut db = Db::open(store.clone(), all).await?;
    db.execute("CREATE TABLE other(x)").await?;
    db.save().await?;
    let mut db = Db::open(store.clone(), StorageOptions::default()).await?;
    db.execute("CREATE TABLE another(x)").await?;
    db.save().await?;
    Ok(store)
}

/// Page reads happen during the query, not at open.
fn no_prefetch() -> StorageOptions {
    StorageOptions {
        prefetch_pages: 0,
        hint_pages: 0,
        ..StorageOptions::default()
    }
}

#[test]
fn backend_read_error_reaches_caller_and_saved_state_survives() -> Result<()> {
    futures::executor::block_on(async {
        let store = populated().await?;
        store.fail_reads.store(true, Ordering::SeqCst);
        let mut db = Db::open(store.clone(), no_prefetch()).await?;
        match db.query("SELECT n FROM t").await {
            Ok(_) => panic!("expected injected failure"),
            Err(e) => assert!(e.to_string().contains("injected page-read failure")),
        }
        drop(db);
        store.fail_reads.store(false, Ordering::SeqCst);
        let mut db = Db::open(store, StorageOptions::default()).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        Ok(())
    })
}

#[test]
fn cancellation_during_pending_kv_read_poison_session_without_publishing() -> Result<()> {
    futures::executor::block_on(async {
        let store = populated().await?;
        let mut db = Db::open(store.clone(), no_prefetch()).await?;
        let (started, waiting) = oneshot::channel();
        *store.gate.lock().unwrap() = Some(started);
        store.inner.reset_stats();
        // The host can poll another future while the engine waits for a page.
        // Cancel precisely after the backend has entered a pending read.
        match select(db.execute("UPDATE t SET n=99").boxed(), waiting).await {
            Either::Left(_) => panic!("query unexpectedly finished before its gated page read"),
            Either::Right((signal, pending_query)) => {
                signal?;
                drop(pending_query);
            }
        }
        assert!(
            db.query("SELECT 1")
                .await
                .unwrap_err()
                .to_string()
                .contains("the session ended")
        );
        drop(db);
        assert_eq!(store.inner.stats().puts, 0);
        let mut db = Db::open(store, StorageOptions::default()).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        Ok(())
    })
}

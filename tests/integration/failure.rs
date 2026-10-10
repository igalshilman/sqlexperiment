use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either, select},
};
use sqlite_lazy_kv::{Result, Session, kv::KvStore, simulation::MemoryKv};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct ControlledKv {
    inner: MemoryKv,
    fail_reads: AtomicBool,
    gate: Mutex<Option<oneshot::Sender<()>>>,
}

impl KvStore for ControlledKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            if key.contains("/block/") {
                if self.fail_reads.load(Ordering::SeqCst) {
                    return Err(io::Error::other("injected block-read failure"));
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
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        self.inner.put(key, value)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.inner.delete(key)
    }
}

async fn populated() -> Result<Arc<ControlledKv>> {
    let store = Arc::new(ControlledKv {
        inner: MemoryKv::default(),
        fail_reads: AtomicBool::new(false),
        gate: Mutex::new(None),
    });
    let mut db = Session::open(store.clone()).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
        .await?;
    db.execute("INSERT INTO t VALUES(1,10)").await?;
    db.checkpoint().await?;
    db.save().await?;
    Ok(store)
}

#[test]
fn backend_read_error_reaches_caller_and_saved_state_survives() -> Result<()> {
    futures::executor::block_on(async {
        let store = populated().await?;
        store.fail_reads.store(true, Ordering::SeqCst);
        match Session::open(store.clone()).await {
            Ok(_) => panic!("expected injected failure"),
            Err(e) => assert!(e.to_string().contains("injected block-read failure")),
        }
        store.fail_reads.store(false, Ordering::SeqCst);
        let mut db = Session::open(store).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        Ok(())
    })
}

#[test]
fn cancellation_during_pending_kv_read_poison_session_without_publishing() -> Result<()> {
    futures::executor::block_on(async {
        let store = populated().await?;
        let mut db = Session::open(store.clone()).await?;
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
                .contains("poisoned")
        );
        drop(db);
        assert_eq!(store.inner.stats().puts, 0);
        let mut db = Session::open(store).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        Ok(())
    })
}

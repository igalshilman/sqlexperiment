use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either, select},
};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use turso_lazy_kv::{
    MapLayout, Result, Session, StorageOptions,
    kv::{KvStore, MemoryKv},
};

const HEAD: &str = "v7/head";
const PENDING: &str = "v7/pending_save";
struct FaultKv {
    inner: MemoryKv,
    calls: AtomicUsize,
    at: usize,
    effect: u8,
    cancel: bool,
    keys: Mutex<Vec<String>>,
    signal: Mutex<Option<oneshot::Sender<()>>>,
}
impl FaultKv {
    fn new(
        inner: MemoryKv,
        at: usize,
        effect: u8,
        cancel: bool,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                inner,
                calls: AtomicUsize::new(0),
                at,
                effect,
                cancel,
                keys: Mutex::default(),
                signal: Mutex::new(Some(tx)),
            }),
            rx,
        )
    }
    async fn mutate(&self, key: &str, value: Option<Vec<u8>>) -> io::Result<()> {
        self.keys.lock().unwrap().push(key.to_owned());
        let fail = self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.at;
        if fail && self.effect == 1 {
            match value {
                Some(v) => self.inner.simulate_unacknowledged_put(key, v),
                None => self.inner.simulate_unacknowledged_delete(key),
            }
        } else if !fail || self.effect == 2 {
            match value {
                Some(v) => self.inner.put(key, v).await?,
                None => self.inner.delete(key).await?,
            }
        }
        if fail {
            if self.cancel {
                let _ = self.signal.lock().unwrap().take().unwrap().send(());
                futures::future::pending::<()>().await;
            }
            return Err(io::Error::other("injected recovery failure"));
        }
        Ok(())
    }
}
impl KvStore for FaultKv {
    fn get<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        self.inner.get(k)
    }
    fn put<'a>(&'a self, k: &'a str, v: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(self.mutate(k, Some(v)))
    }
    fn delete<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(self.mutate(k, None))
    }
}
async fn changed(store: Arc<dyn KvStore>, layout: MapLayout, empty: bool) -> Result<Session> {
    let mut db = Session::open_with_options(
        store,
        StorageOptions {
            layout,
            write_concurrency: 8,
        },
    )
    .await?;
    if empty {
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, payload TEXT)")
            .await?;
        db.execute("INSERT INTO t VALUES(1, 10, printf('%09000d',1))")
            .await?;
    }
    db.execute("UPDATE t SET n=20, payload=printf('%09000d',2)")
        .await?;
    Ok(db)
}
async fn check(store: Arc<MemoryKv>, expected: i32) -> Result<()> {
    let mut db = Session::open(store.clone()).await?;
    if expected == 0 {
        assert_eq!(
            db.query("SELECT count(*) FROM sqlite_master").await?[0][0].to_string(),
            "0"
        );
    } else {
        assert_eq!(
            db.query("SELECT n FROM t").await?[0][0].to_string(),
            expected.to_string()
        );
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
    }
    assert!(store.get(PENDING).await?.is_none());
    Ok(())
}

#[test]
fn every_recovery_mutation_is_restartable_for_abort_and_commit() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            for empty in [false, true] {
                let base = MemoryKv::default();
                if !empty {
                    let mut db = changed(Arc::new(base.clone()), layout, true).await?;
                    db.execute("UPDATE t SET n=10").await?;
                    db.save().await?;
                }
                let (probe, _) = FaultKv::new(base.fork(), usize::MAX, 0, false);
                changed(probe.clone(), layout, empty).await?.save().await?;
                let commit_at = probe
                    .keys
                    .lock()
                    .unwrap()
                    .iter()
                    .position(|k| k == HEAD)
                    .unwrap()
                    + 1;
                let final_keys = probe.inner.keys();
                let final_bytes = probe.inner.stored_bytes();
                // Abort before publication, visible-but-unconfirmed commit,
                // and acknowledged commit interrupted during cleanup.
                for (stage, at, effect) in [
                    ("abort", 2, 1),
                    ("uncertain", commit_at, 1),
                    ("committed", commit_at + 1, 0),
                ] {
                    let (interrupted, _) = FaultKv::new(base.fork(), at, effect, false);
                    let result = changed(interrupted.clone(), layout, empty)
                        .await?
                        .save()
                        .await;
                    if stage == "committed" {
                        assert!(result?.cleanup_error.is_some());
                    } else {
                        assert!(result.is_err());
                    }
                    let pending_base = interrupted.inner.clone();
                    let (recovery_probe, _) =
                        FaultKv::new(pending_base.fork(), usize::MAX, 0, false);
                    drop(Session::open(recovery_probe.clone()).await?);
                    let count = recovery_probe.calls.load(Ordering::SeqCst);
                    assert!(count >= 2);
                    let keys = recovery_probe.keys.lock().unwrap().clone();
                    assert_eq!(keys[0], HEAD);
                    assert_eq!(keys.last().unwrap(), PENDING);
                    for fail_at in 1..=count {
                        for effect in 0..3 {
                            for cancel in [false, true] {
                                for persist in [false, true] {
                                    let (fault, gate) =
                                        FaultKv::new(pending_base.fork(), fail_at, effect, cancel);
                                    if cancel {
                                        match select(Session::open(fault.clone()).boxed(), gate)
                                            .await
                                        {
                                            Either::Left(_) => {
                                                panic!("missed recovery cancellation")
                                            }
                                            Either::Right((signal, future)) => {
                                                signal?;
                                                drop(future);
                                            }
                                        }
                                    } else {
                                        assert!(Session::open(fault.clone()).await.is_err());
                                    }
                                    if fail_at == 1 {
                                        assert_eq!(
                                            fault.keys.lock().unwrap().len(),
                                            1,
                                            "deleted before head confirmation"
                                        );
                                    }
                                    // Include the original uncertain publication too, not
                                    // just the recovery operation that was interrupted.
                                    let extra = if persist {
                                        let mut e = keys.clone();
                                        e.push(HEAD.into());
                                        e
                                    } else {
                                        vec![]
                                    };
                                    fault.inner.crash(&extra);
                                    let committed = stage == "committed"
                                        || (stage == "uncertain"
                                            && (fail_at > 1 || effect == 2 || persist));
                                    check(
                                        Arc::new(fault.inner.clone()),
                                        if committed {
                                            20
                                        } else if empty {
                                            0
                                        } else {
                                            10
                                        },
                                    )
                                    .await?;
                                    assert_eq!(
                                        fault.inner.keys(),
                                        if committed {
                                            final_keys.clone()
                                        } else {
                                            base.keys()
                                        },
                                        "leaked objects after {stage} recovery"
                                    );
                                    assert_eq!(
                                        fault.inner.stored_bytes(),
                                        if committed {
                                            final_bytes
                                        } else {
                                            base.stored_bytes()
                                        }
                                    );
                                    fault.inner.crash(&[]);
                                    check(
                                        Arc::new(fault.inner.clone()),
                                        if committed {
                                            20
                                        } else if empty {
                                            0
                                        } else {
                                            10
                                        },
                                    )
                                    .await?;
                                    // A retry can use the same IDs only after this recovery.
                                    changed(
                                        Arc::new(fault.inner.clone()),
                                        layout,
                                        empty && !committed,
                                    )
                                    .await?
                                    .save()
                                    .await?;
                                    fault.inner.crash(&[]);
                                    check(Arc::new(fault.inner.clone()), 20).await?;
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        println!("RECOVERY: {cases} interrupted recovery cases, including interrupted first saves");
        Ok(())
    })
}

#[test]
fn uncertain_intent_deletion_can_be_replaced_by_next_save_without_a_crash() -> Result<()> {
    futures::executor::block_on(async {
        let (probe, _) = FaultKv::new(MemoryKv::default(), usize::MAX, 0, false);
        changed(probe.clone(), MapLayout::Radix, true)
            .await?
            .save()
            .await?;
        let last = probe.calls.load(Ordering::SeqCst);
        let (fault, _) = FaultKv::new(MemoryKv::default(), last, 1, false);
        assert!(
            changed(fault.clone(), MapLayout::Radix, true)
                .await?
                .save()
                .await?
                .cleanup_error
                .is_some()
        );
        assert!(fault.inner.get(PENDING).await?.is_none()); // durable intent may remain
        let mut db = Session::open(Arc::new(fault.inner.clone())).await?;
        db.execute("UPDATE t SET n=30").await?;
        assert!(db.save().await?.cleanup_error.is_none());
        fault.inner.crash(&[]);
        check(Arc::new(fault.inner.clone()), 30).await?;
        Ok(())
    })
}

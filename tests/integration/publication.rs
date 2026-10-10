use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Either, select},
};
use sqlite_kv::{MapLayout, Result, Session, StorageOptions, kv::KvStore, simulation::MemoryKv};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct FaultKv {
    inner: MemoryKv,
    calls: AtomicUsize,
    fail_at: usize,
    effect: u8, // 0: absent, 1: visible only, 2: durable before interruption
    cancel: bool,
    reached: Mutex<Option<oneshot::Sender<()>>>,
    keys: Mutex<Vec<String>>,
}
impl FaultKv {
    async fn interrupt(&self) -> io::Result<()> {
        if self.cancel {
            let signal = self.reached.lock().unwrap().take().unwrap();
            let _ = signal.send(());
            futures::future::pending::<()>().await;
        }
        Err(io::Error::other("injected publication failure"))
    }
}
impl KvStore for FaultKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        self.inner.get(key)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.keys.lock().unwrap().push(format!("delete:{key}"));
            let fail = call == self.fail_at;
            if fail && self.effect == 1 {
                self.inner.simulate_unacknowledged_delete(key);
            } else if !fail || self.effect == 2 {
                self.inner.delete(key).await?;
            }
            if fail {
                self.interrupt().await?;
            }
            Ok(())
        })
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.keys.lock().unwrap().push(key.to_owned());
            let fail = call == self.fail_at;
            if !fail || self.effect > 0 {
                if key.starts_with("v9/block/") || key.starts_with("v9/node/") {
                    let id = u64::from_str_radix(key.rsplit('/').next().unwrap(), 16).unwrap();
                    let high = self
                        .inner
                        .get("v9/head")
                        .await?
                        .map(|v| u64::from_le_bytes(v[12..20].try_into().unwrap()))
                        .unwrap_or(0);
                    assert!(id > high, "overwrote committed range: {id} <= {high}");
                }
                if fail && self.effect == 1 {
                    self.inner.simulate_unacknowledged_put(key, value);
                } else {
                    self.inner.put(key, value).await?;
                }
            }
            if fail {
                return self.interrupt().await;
            }
            Ok(())
        })
    }
}

async fn baseline(layout: MapLayout) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db = Session::open_with_options(
        Arc::new(store.clone()),
        StorageOptions {
            layout,
            ..StorageOptions::default()
        },
    )
    .await?;
    populate(&mut db).await?;
    // save itself checkpoints: the durable baseline has no WAL.
    db.save().await?;
    Ok(store)
}

async fn populate(db: &mut Session) -> Result<()> {
    db.execute("CREATE TABLE balances(id INTEGER PRIMARY KEY, n INTEGER, payload TEXT)")
        .await?;
    db.execute("CREATE INDEX by_n ON balances(n)").await?;
    db.execute("CREATE TABLE audit(n INTEGER)").await?;
    db.execute("INSERT INTO balances VALUES(1, 10, printf('%09000d',1))")
        .await?;
    db.execute("INSERT INTO audit VALUES(10)").await?;
    Ok(())
}

async fn changed(store: Arc<dyn KvStore>, checkpoint: bool, layout: MapLayout) -> Result<Session> {
    let mut db = Session::open_with_options(
        store,
        StorageOptions {
            layout,
            ..StorageOptions::default()
        },
    )
    .await?;
    if db
        .query("SELECT count(*) FROM sqlite_master WHERE name='balances'")
        .await?[0][0]
        .to_string()
        == "0"
    {
        populate(&mut db).await?;
    }
    db.execute("BEGIN").await?;
    db.execute("UPDATE balances SET n=20, payload=printf('%09000d',2)")
        .await?;
    db.execute("UPDATE audit SET n=20").await?;
    db.execute("COMMIT").await?;
    if checkpoint {
        db.checkpoint().await?;
    }
    Ok(db)
}

async fn check(store: Arc<dyn KvStore>, expected: i32) -> Result<()> {
    let mut db = Session::open(store).await?;
    if expected == 0 {
        assert_eq!(
            db.query("SELECT count(*) FROM sqlite_master").await?[0][0].to_string(),
            "0"
        );
        return Ok(());
    }
    let row = db
        .query(
            "SELECT balances.n, audit.n, substr(payload,-1), length(payload) FROM balances, audit",
        )
        .await?;
    assert_eq!(row[0][0].to_string(), expected.to_string());
    assert_eq!(row[0][1].to_string(), expected.to_string());
    assert_eq!(row[0][2].to_string(), (expected / 10).to_string());
    assert_eq!(row[0][3].to_string(), "9000");
    assert_eq!(
        db.query("PRAGMA integrity_check").await?[0][0].to_string(),
        "ok"
    );
    Ok(())
}

async fn exercise(checkpoint: bool, empty: bool, layout: MapLayout) -> Result<()> {
    let base = if empty {
        MemoryKv::default()
    } else {
        baseline(layout).await?
    };
    let probe = Arc::new(FaultKv {
        inner: base.fork(),
        calls: AtomicUsize::new(0),
        fail_at: usize::MAX,
        effect: 0,
        cancel: false,
        reached: Mutex::new(None),
        keys: Mutex::default(),
    });
    changed(probe.clone(), checkpoint, layout)
        .await?
        .save()
        .await?;
    check(probe.clone(), 20).await?;
    let writes = probe.calls.load(Ordering::SeqCst);
    let keys = probe.keys.lock().unwrap().clone();
    assert_eq!(keys.first().unwrap(), "v9/pending_save");
    assert_eq!(keys.last().unwrap(), "delete:v9/pending_save");
    let commit_at = keys.iter().position(|k| k == "v9/head").unwrap() + 1;
    probe.inner.crash(&[]);
    check(Arc::new(probe.inner.clone()), 20).await?;
    assert!(writes > 1);
    for fail_at in 1..=writes {
        for effect in 0..3 {
            for cancel in [false, true] {
                for persist_failed in [false, true] {
                    let (reached, gate) = oneshot::channel();
                    let store = Arc::new(FaultKv {
                        inner: base.fork(),
                        calls: AtomicUsize::new(0),
                        fail_at,
                        effect,
                        cancel,
                        reached: Mutex::new(Some(reached)),
                        keys: Mutex::default(),
                    });
                    let db = changed(store.clone(), checkpoint, layout).await?;
                    if cancel {
                        match select(db.save().boxed(), gate).await {
                            Either::Left(_) => panic!("save did not reach injected cancellation"),
                            Either::Right((signal, pending)) => {
                                signal?;
                                drop(pending);
                            }
                        }
                    } else {
                        let result = db.save().await;
                        if fail_at <= commit_at {
                            assert!(
                                result
                                    .unwrap_err()
                                    .to_string()
                                    .contains("injected publication failure")
                            );
                        } else {
                            assert!(
                                result?
                                    .cleanup_error
                                    .unwrap()
                                    .to_string()
                                    .contains("injected publication failure")
                            );
                        }
                    }
                    // A visible-only failed mutation may either survive or disappear.
                    // All acknowledged mutations must survive both crash variants.
                    let extra = if persist_failed {
                        vec![
                            keys[fail_at - 1]
                                .strip_prefix("delete:")
                                .unwrap_or(&keys[fail_at - 1])
                                .to_owned(),
                        ]
                    } else {
                        Vec::new()
                    };
                    store.inner.crash(&extra);
                    let published = fail_at > commit_at
                        || (fail_at == commit_at
                            && (effect == 2 || (effect == 1 && persist_failed)));
                    let expected = if published {
                        20
                    } else if empty {
                        0
                    } else {
                        10
                    };
                    // Fresh engine and metadata cache; only backend bytes survive.
                    check(store.clone(), expected).await?;
                    assert_eq!(
                        store.inner.keys(),
                        if published {
                            probe.inner.keys()
                        } else {
                            base.keys()
                        },
                        "recovery leaked objects"
                    );
                    // A later acknowledged save must succeed after every interruption.
                    let high = store
                        .inner
                        .get("v9/head")
                        .await?
                        .map(|v| u64::from_le_bytes(v[12..20].try_into().unwrap()))
                        .unwrap_or(0);
                    store.keys.lock().unwrap().clear();
                    changed(store.clone(), checkpoint, layout)
                        .await?
                        .save()
                        .await?;
                    for key in store
                        .keys
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|k| k.starts_with("v9/block/") || k.starts_with("v9/node/"))
                    {
                        let id = u64::from_str_radix(key.rsplit('/').next().unwrap(), 16).unwrap();
                        assert!(
                            id > high,
                            "allocation overwrote committed range: {id} <= {high}"
                        );
                    }
                    store.inner.crash(&[]);
                    check(Arc::new(store.inner.clone()), 20).await?;
                }
            }
        }
    }
    println!(
        "layout={layout:?}, checkpoint={checkpoint}, empty={empty}: {} crash/reopen cases across {writes} mutations",
        writes * 12
    );
    Ok(())
}

async fn all_layouts(checkpoint: bool, empty: bool) -> Result<()> {
    for layout in [
        MapLayout::Radix,
        MapLayout::InlineRoot,
        MapLayout::HeadOverrides,
    ] {
        exercise(checkpoint, empty, layout).await?;
    }
    Ok(())
}
#[test]
fn every_publication_put_with_automatic_checkpoint() -> Result<()> {
    futures::executor::block_on(all_layouts(false, false))
}
#[test]
fn every_publication_put_after_explicit_checkpoint() -> Result<()> {
    futures::executor::block_on(all_layouts(true, false))
}
#[test]
fn every_first_publication_put() -> Result<()> {
    futures::executor::block_on(all_layouts(false, true))
}

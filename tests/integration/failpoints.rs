use futures::{
    FutureExt,
    future::{Either, select},
};
use sqlite_kv::{
    MapLayout, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation},
};
use std::sync::Arc;

const LAYOUTS: [MapLayout; 3] = [
    MapLayout::Radix,
    MapLayout::InlineRoot,
    MapLayout::HeadOverrides,
];
async fn baseline(layout: MapLayout) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db = Session::open_with_options(
        Arc::new(store.clone()),
        StorageOptions {
            layout,
            write_concurrency: 8,
            ..StorageOptions::default()
        },
    )
    .await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, payload TEXT)")
        .await?;
    db.execute("CREATE INDEX by_n ON t(n)").await?;
    db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<30) INSERT INTO t SELECT x,x,printf('%09000d',x) FROM n").await?;
    assert!(db.save().await?.cleanup_error.is_none());
    Ok(store)
}
fn plan(operation: Operation, prefix: &str, nth: usize, effect: Effect, cancel: bool) -> Failpoint {
    Failpoint {
        operation,
        key_prefix: prefix.into(),
        nth,
        effect,
        cancel,
    }
}
async fn check(store: Arc<dyn KvStore>, changed: bool) -> Result<()> {
    let mut db = Session::open(store).await?;
    let rows = db
        .query("SELECT id,n,length(payload),substr(payload,-2) FROM t ORDER BY id")
        .await?;
    assert_eq!(rows.len(), 30);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0].as_int(), Some(i as i64 + 1));
        assert_eq!(
            row[1].as_int(),
            Some(if changed { 99 } else { i as i64 + 1 })
        );
        assert_eq!(row[2].as_int(), Some(9000));
        assert_eq!(
            row[3].to_string(),
            if changed {
                "00".into()
            } else {
                format!("{:02}", i + 1)
            }
        );
    }
    assert_eq!(
        db.query("PRAGMA integrity_check").await?[0][0].to_string(),
        "ok"
    );
    Ok(())
}

#[test]
fn every_read_can_error_or_cancel_before_or_after_delivery() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for layout in LAYOUTS {
            let base = baseline(layout).await?;
            for sql in [
                None,
                Some("SELECT payload FROM t WHERE id=15"),
                Some("SELECT payload FROM t WHERE id BETWEEN 13 AND 17"),
                Some("SELECT id FROM t INDEXED BY by_n WHERE n BETWEEN 10 AND 20"),
                Some("SELECT payload FROM t NOT INDEXED ORDER BY id"),
                Some("UPDATE t SET n=99 WHERE id=15"),
            ] {
                let probe = Arc::new(FaultKv::new(base.fork(), 7));
                if let Some(sql) = sql {
                    let mut db = Session::open(probe.clone()).await?;
                    let _gate =
                        probe.arm(plan(Operation::Get, "", usize::MAX, Effect::None, false));
                    db.query(sql).await?;
                } else {
                    let _gate =
                        probe.arm(plan(Operation::Get, "", usize::MAX, Effect::None, false));
                    drop(Session::open(probe.clone()).await?);
                }
                let count = probe
                    .events()
                    .iter()
                    .filter(|e| e.operation == Operation::Get)
                    .count();
                assert!(count > 0);
                for nth in 1..=count {
                    for effect in [Effect::None, Effect::After] {
                        for cancel in [false, true] {
                            let store = Arc::new(FaultKv::new(base.fork(), nth as u64));
                            let mut session = if sql.is_some() {
                                Some(Session::open(store.clone()).await?)
                            } else {
                                None
                            };
                            store.inner.reset_stats();
                            let gate = store.arm(plan(Operation::Get, "", nth, effect, cancel));
                            let future = async {
                                if let Some(sql) = sql {
                                    session.as_mut().unwrap().query(sql).await.map(|_| ())
                                } else {
                                    Session::open(store.clone()).await.map(drop)
                                }
                            }
                            .boxed();
                            if cancel {
                                match select(future, gate).await {
                                    Either::Left(_) => panic!("read failpoint not reached"),
                                    Either::Right((signal, pending)) => {
                                        signal?;
                                        drop(pending);
                                    }
                                }
                            } else {
                                assert!(future.await.is_err());
                            }
                            if let Some(mut db) = session {
                                assert!(
                                    db.query("SELECT 1")
                                        .await
                                        .unwrap_err()
                                        .to_string()
                                        .contains("poisoned")
                                );
                                drop(db);
                            }
                            assert_eq!(store.active(), 0);
                            assert_eq!(store.events().iter().filter(|e| e.faulted).count(), 1);
                            assert_eq!(
                                (store.inner.stats().puts, store.inner.stats().deletes),
                                (0, 0)
                            );
                            store.disarm();
                            store.inner.crash(&[]);
                            assert_eq!(store.inner.keys(), base.keys());
                            assert_eq!(store.inner.stored_bytes(), base.stored_bytes());
                            check(store, false).await?;
                            cases += 1;
                        }
                    }
                }
            }
        }
        println!("read failpoints: {cases} cases (all observed gets; 3 layouts, 6 workloads)");
        Ok(())
    })
}

#[test]
fn reordered_object_errors_drain_before_recovery() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for layout in LAYOUTS {
            let base = baseline(layout).await?;
            for concurrency in [1, 2, 8] {
                for seed in [1, 7, 31] {
                    for effect in [Effect::None, Effect::Visible, Effect::After] {
                        let store = Arc::new(FaultKv::new(base.fork(), seed));
                        let mut db = Session::open_with_options(
                            store.clone(),
                            StorageOptions {
                                layout,
                                write_concurrency: concurrency,
                                ..StorageOptions::default()
                            },
                        )
                        .await?;
                        db.execute("UPDATE t SET n=99,payload=printf('%09000d',0)")
                            .await?;
                        let _gate = store.arm(plan(Operation::Put, "v9/block/", 3, effect, false));
                        assert!(db.save().await.is_err());
                        assert_eq!(store.active(), 0);
                        assert!(store.max_active() <= concurrency);
                        if concurrency > 1 {
                            assert!(store.max_active() > 1);
                        }
                        assert!(
                            !store
                                .events()
                                .iter()
                                .any(|e| e.operation == Operation::Put && e.key == "v9/head")
                        );
                        store.disarm();
                        store.inner.crash(&[]);
                        check(store.clone(), false).await?;
                        assert_eq!(store.inner.keys(), base.keys());
                        assert_eq!(store.inner.stored_bytes(), base.stored_bytes());
                        cases += 1;
                    }
                }
            }
        }
        println!(
            "reordered object errors: {cases} cases (3 layouts, concurrency 1/2/8, 3 schedules, 3 outcomes)"
        );
        Ok(())
    })
}

#[test]
fn recovery_read_failures_do_not_mutate_and_repeated_cleanup_failures_are_retryable() -> Result<()>
{
    futures::executor::block_on(async {
        let mut read_cases = 0;
        let mut cleanup_cases = 0;
        for layout in LAYOUTS {
            let base = baseline(layout).await?;
            for committed in [false, true] {
                let pending = Arc::new(FaultKv::new(base.fork(), 7));
                let mut db = Session::open(pending.clone()).await?;
                db.execute("UPDATE t SET n=99,payload=printf('%09000d',0)")
                    .await?;
                let _gate = pending.arm(plan(
                    Operation::Put,
                    "v9/head",
                    1,
                    if committed {
                        Effect::After
                    } else {
                        Effect::None
                    },
                    false,
                ));
                assert!(db.save().await.is_err());
                pending.disarm();
                pending.inner.crash(&[]);
                for nth in [1, 2] {
                    for effect in [Effect::None, Effect::After] {
                        for cancel in [false, true] {
                            let store = Arc::new(FaultKv::new(pending.inner.fork(), 31));
                            let gate = store.arm(plan(Operation::Get, "", nth, effect, cancel));
                            let future = Session::open(store.clone()).boxed();
                            if cancel {
                                match select(future, gate).await {
                                    Either::Left(_) => {
                                        panic!("recovery read failpoint not reached")
                                    }
                                    Either::Right((signal, pending)) => {
                                        signal?;
                                        drop(pending);
                                    }
                                }
                            } else {
                                assert!(future.await.is_err());
                            }
                            assert_eq!(store.active(), 0);
                            assert_eq!(
                                (store.inner.stats().puts, store.inner.stats().deletes),
                                (0, 0)
                            );
                            store.disarm();
                            check(store, committed).await?;
                            read_cases += 1;
                        }
                    }
                }
                // Interrupt successive recoveries, including an acknowledged
                // deletion whose response is lost. Every retry may delete absent keys.
                for attempt in 0..5 {
                    let _gate = pending.arm(plan(
                        Operation::Delete,
                        "v9/block/",
                        1,
                        if attempt % 2 == 0 {
                            Effect::After
                        } else {
                            Effect::Visible
                        },
                        false,
                    ));
                    assert!(Session::open(pending.clone()).await.is_err());
                    assert_eq!(pending.active(), 0);
                    assert!(pending.inner.keys().contains("v9/pending_save"));
                    pending.disarm();
                    pending.inner.crash(&[]);
                    cleanup_cases += 1;
                }
                check(pending.clone(), committed).await?;
                assert!(!pending.inner.keys().contains("v9/pending_save"));
                if !committed {
                    assert_eq!(pending.inner.keys(), base.keys());
                    assert_eq!(pending.inner.stored_bytes(), base.stored_bytes());
                }
            }
        }
        println!(
            "recovery read failpoints: {read_cases} cases; repeated cleanup interruptions: {cleanup_cases}"
        );
        Ok(())
    })
}

#[test]
fn counters_distinguish_request_bytes_reclamation_and_missing_keys() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let key = "v9/block/0000000000000001";
        assert!(store.get(key).await?.is_none());
        store.put(key, vec![1; 4096]).await?;
        store.get(key).await?;
        store.get(key).await?;
        store.delete(key).await?;
        store.delete(key).await?;
        let s = store.stats();
        let t = &s.by_kind["block"];
        assert_eq!((t.gets, t.misses, t.puts, t.deletes), (3, 1, 1, 2));
        assert_eq!(
            (t.bytes_read, t.bytes_written, t.bytes_deleted),
            (8192, 4096, 4096)
        );
        assert_eq!(
            (t.get_key_bytes, t.put_key_bytes, t.delete_key_bytes),
            (75, 25, 50)
        );
        assert_eq!(
            (s.read_keys.len(), s.write_keys.len(), s.delete_keys.len()),
            (1, 1, 1)
        );
        assert_eq!((store.stored_bytes(), store.stored_key_bytes()), (0, 0));
        store.reset_stats();
        assert!(store.stats().by_kind.is_empty());
        Ok(())
    })
}

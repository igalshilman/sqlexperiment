//! `Durability::Commit`: every SQL commit is durable when its statement returns.
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions,
    simulation::{
        Effect, Failpoint, FaultKv, LatencyKv, MemoryKv, MonitorKv, Operation, fresh_namespace,
        logged_transactions,
    },
};
use std::sync::Arc;

fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        durability: Durability::Commit,
        ..StorageOptions::default()
    }
}

async fn count(db: &mut Session) -> Result<i64> {
    let rows = db
        .query("SELECT count(*), coalesce(sum(n), 0) FROM t")
        .await?;
    let count = rows[0][0].as_int().unwrap();
    assert_eq!(rows[0][1].as_int(), Some(0), "pairs commit together");
    Ok(count)
}

#[test]
fn statements_are_durable_without_save() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        assert_eq!(
            store.stats().kind("commit").puts,
            1,
            "one commit per statement"
        );
        db.execute("INSERT INTO t VALUES(1, 0)").await?;
        assert_eq!(store.stats().kind("commit").puts, 2);
        db.query("SELECT * FROM t").await?;
        assert_eq!(store.stats().kind("commit").puts, 2, "reads commit nothing");
        drop(db);
        store.crash(&[]);

        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(count(&mut db).await?, 1);
        Ok(())
    })
}

#[test]
fn a_transaction_commits_as_a_whole_or_not_at_all() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        let before = store.stats().kind("commit").puts;
        let id = db
            .transaction(async |db| {
                db.execute("INSERT INTO t VALUES(1, 5)").await?;
                db.execute("INSERT INTO t VALUES(2, -5)").await?;
                Ok(2)
            })
            .await?;
        assert_eq!(id, 2);
        assert_eq!(
            store.stats().kind("commit").puts,
            before + 1,
            "one log transaction"
        );

        let failed: Result<()> = db
            .transaction(async |db| {
                db.execute("INSERT INTO t VALUES(3, 7)").await?;
                Err("application error".into())
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(store.stats().kind("commit").puts, before + 1, "rolled back");
        assert_eq!(count(&mut db).await?, 2, "the session continues");

        // An open transaction is all that dropping a session loses.
        db.execute("BEGIN").await?;
        db.execute("INSERT INTO t VALUES(4, 1)").await?;
        drop(db);
        store.crash(&[]);
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(count(&mut db).await?, 2);
        Ok(())
    })
}

/// A long session crosses checkpoints, garbage collection and the cache's
/// claims, reading its own writes throughout.
#[test]
fn a_long_session_checkpoints_and_reads_its_writes() -> Result<()> {
    futures::executor::block_on(async {
        for cache in [None, Some(fresh_namespace())] {
            let store = Arc::new(MemoryKv::default());
            let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
            db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)")
                .await?;
            for i in 0..60i64 {
                db.transaction(async |db| {
                    db.execute(&format!(
                        "INSERT INTO t VALUES({}, {i}, zeroblob(900))",
                        2 * i
                    ))
                    .await?;
                    db.execute(&format!("INSERT INTO t VALUES({}, {}, 'x')", 2 * i + 1, -i))
                        .await
                })
                .await?;
                assert_eq!(count(&mut db).await?, 2 * (i + 1));
            }
            let logs = store.keys().iter().filter(|k| k.contains("/log/")).count();
            assert!(
                logs < 12,
                "garbage is collected as the session goes: {logs}"
            );
            assert!(
                store.stats().kind("page").puts > 0,
                "the log was checkpointed"
            );
            drop(db);
            store.crash(&[]);

            let mut db = Session::open_with_options(store.clone(), options(cache)).await?;
            assert_eq!(count(&mut db).await?, 120);
            assert_eq!(
                db.query("PRAGMA integrity_check").await?[0][0].to_string(),
                "ok"
            );
        }
        Ok(())
    })
}

/// With an owner cache, a write transaction costs one round trip until the
/// log is long enough to checkpoint.
#[test]
fn a_commit_is_one_round_trip_with_a_cache() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let options = StorageOptions {
            checkpoint_after: 32,
            ..options(Some(cache))
        };
        let mut db = Session::open_with_options(store.clone(), options).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        for i in 0..20 {
            store.reset();
            db.execute(&format!("INSERT INTO t VALUES({i}, 0)")).await?;
            assert_eq!(store.round_trips(), 1, "commit {i}");
        }
        store.reset();
        assert_eq!(count(&mut db).await?, 20);
        assert_eq!(store.round_trips(), 0, "reads are served locally");
        Ok(())
    })
}

/// Fail every mutation of a commit-per-transaction run in turn. Every
/// transaction acknowledged before the failure survives a crash, the failed one
/// may or may not, and nothing else appears.
#[test]
fn every_acknowledged_commit_survives_any_single_failure() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for cached in [false, true] {
            for effect in [Effect::None, Effect::After] {
                for operation in [Operation::Put, Operation::Delete] {
                    for nth in 1.. {
                        if !run_case(cached, effect, operation, nth).await? {
                            break;
                        }
                        cases += 1;
                    }
                }
            }
        }
        println!("commit campaign: {cases} cases");
        assert!(cases > 100, "{cases} cases");
        Ok(())
    })
}

/// Returns whether the failpoint was reached.
async fn run_case(cached: bool, effect: Effect, operation: Operation, nth: usize) -> Result<bool> {
    let cache = cached.then(fresh_namespace);
    let store = Arc::new(MonitorKv::new(FaultKv::new(
        MemoryKv::default(),
        nth as u64,
    )));
    let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)")
        .await?;
    let _signal = store.arm(Failpoint {
        operation,
        key_prefix: "v14/".into(),
        nth,
        effect,
        cancel: false,
    });
    let mut acknowledged = 0;
    let mut failed = false;
    for i in 0..30i64 {
        let result = db
            .transaction(async |db| {
                db.execute(&format!(
                    "INSERT INTO t VALUES({}, {i}, zeroblob(700))",
                    2 * i
                ))
                .await?;
                db.execute(&format!("INSERT INTO t VALUES({}, {}, 'x')", 2 * i + 1, -i))
                    .await
            })
            .await;
        match result {
            Ok(()) => acknowledged += 2,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    let reached = store.events().iter().any(|event| event.faulted);
    drop(db);
    store.disarm();
    store.inner.inner.crash(&[]);

    // Reopen through the same owner cache, as a restarted handler would.
    let mut db = Session::open_with_options(store.clone(), options(cache)).await?;
    let survived = count(&mut db).await?;
    assert!(
        survived == acknowledged || (failed && survived == acknowledged + 2),
        "{effect:?} {operation:?} #{nth} cached={cached}: {survived} rows, {acknowledged} acknowledged"
    );
    assert_eq!(
        db.query("PRAGMA integrity_check").await?[0][0].to_string(),
        "ok"
    );
    // The namespace stays writable.
    db.execute("INSERT INTO t VALUES(1000, 0, 'y')").await?;
    drop(db);
    let mut db = Session::open_with_options(store.clone(), options(None)).await?;
    assert_eq!(count(&mut db).await?, survived + 1);
    assert_eq!(
        store.violations(),
        Vec::<String>::new(),
        "{effect:?} {operation:?} #{nth}"
    );
    Ok(reached)
}

/// A string of several statements runs all of them, each under the same rules
/// as on its own, and returns the rows of the last.
#[test]
fn every_statement_of_a_string_runs() -> Result<()> {
    futures::executor::block_on(async {
        for durability in [Durability::Save, Durability::Commit] {
            let store = Arc::new(MemoryKv::default());
            let options = StorageOptions {
                durability,
                ..StorageOptions::default()
            };
            let mut db = Session::open_with_options(store.clone(), options.clone()).await?;
            let rows = db
                .query(
                    "CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER);
                     INSERT INTO t VALUES(1, 0); INSERT INTO t VALUES(2, 0);
                     SELECT count(*) FROM t; -- trailing comment",
                )
                .await?;
            assert_eq!(rows[0][0].as_int(), Some(2));
            db.execute("BEGIN; INSERT INTO t VALUES(3, 0); COMMIT")
                .await?;
            assert!(db.execute("  -- nothing\n ").await.is_err());
            if durability == Durability::Save {
                db.save().await?;
            } else {
                drop(db);
            }
            store.crash(&[]);
            let mut db = Session::open_with_options(store, options).await?;
            assert_eq!(count(&mut db).await?, 3, "{durability:?}");
        }
        Ok(())
    })
}

/// A transaction body may not end its transaction: the statement fails, the
/// session is poisoned, and nothing of the body becomes durable.
#[test]
fn a_transaction_body_cannot_commit_itself() -> Result<()> {
    futures::executor::block_on(async {
        for end in ["COMMIT", "END", "ROLLBACK", "COMMIT; BEGIN"] {
            let store = Arc::new(MemoryKv::default());
            let mut db = Session::open_with_options(store.clone(), options(None)).await?;
            db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
                .await?;
            let result: Result<()> = db
                .transaction(async |db| {
                    db.execute("INSERT INTO t VALUES(1, 0)").await?;
                    db.execute(end).await?;
                    Ok(())
                })
                .await;
            assert!(result.is_err(), "{end}");
            assert!(db.execute("SELECT 1").await.is_err(), "{end}: poisoned");
            drop(db);
            store.crash(&[]);
            let mut db = Session::open_with_options(store.clone(), options(None)).await?;
            assert_eq!(count(&mut db).await?, 0, "{end}");
        }

        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open_with_options(store, options(None)).await?;
        let nested: Result<()> = db
            .transaction(async |db| db.transaction(async |_| Ok(())).await)
            .await;
        assert!(nested.unwrap_err().to_string().contains("do not nest"));
        Ok(())
    })
}

/// Dropping a transaction future while its body awaits something else (a call
/// to another service, a timeout) leaves its SQL transaction open; the session
/// must not accept more work that would silently join it.
#[test]
fn an_abandoned_transaction_poisons_the_session() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        {
            let (_send, receive) = futures::channel::oneshot::channel::<()>();
            let mut transaction = Box::pin(db.transaction(async move |db| {
                db.execute("INSERT INTO t VALUES(1, 0)").await?;
                let _ = receive.await;
                Ok(())
            }));
            for _ in 0..1000 {
                assert!(futures::poll!(transaction.as_mut()).is_pending());
            }
        }
        assert!(db.execute("INSERT INTO t VALUES(2, 0)").await.is_err());
        assert!(db.save().await.is_err());
        Ok(())
    })
}

/// A checkpoint failure after a durable commit is reported, not swallowed.
#[test]
fn commit_cleanup_errors_are_reported() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        let _signal = store.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: "v14/page/".into(),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        let mut i = 0;
        while store.events().iter().all(|event| !event.faulted) {
            db.execute(&format!("INSERT INTO t VALUES({i}, 0)")).await?;
            i += 1;
        }
        assert!(db.take_cleanup_error().is_some());
        assert!(db.take_cleanup_error().is_none());
        db.execute("INSERT INTO t VALUES(-1, 0)").await?;
        store.disarm();
        db.execute("INSERT INTO t VALUES(-2, 0)").await?;
        drop(db);
        let mut db = Session::open_with_options(store, options(None)).await?;
        assert_eq!(count(&mut db).await?, i + 2);
        Ok(())
    })
}

/// A session that only reads, open while another commits, keeps reading the
/// state it opened at: checkpoints do not write images it has not replayed.
/// Once it ends, checkpoints catch up.
#[test]
fn an_overlapping_reader_keeps_its_view() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let namespace = fresh_namespace();
        let opts = || StorageOptions {
            prefetch_pages: 0,
            hint_pages: 0,
            ..options(Some(namespace.clone()))
        };
        let mut db = Session::open_with_options(store.clone(), opts()).await?;
        db.execute(
            "CREATE TABLE a(x); CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, pad BLOB)",
        )
        .await?;
        db.transaction(async |db| {
            for i in 0..200 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, 1, zeroblob(100))"))
                    .await?;
            }
            Ok(())
        })
        .await?;
        for i in 0..8 {
            db.execute(&format!("INSERT INTO a VALUES ({i})")).await?;
        }
        drop(db);

        let mut reader = Session::open_with_options(store.clone(), opts()).await?;
        reader.query("SELECT count(*) FROM a").await?;
        let mut writer = Session::open_with_options(store.clone(), opts()).await?;
        for _ in 0..10 {
            writer.execute("UPDATE t SET n = n + 1").await?;
        }
        let rows = reader.query("SELECT count(*), sum(n) FROM t").await?;
        assert_eq!(
            (rows[0][0].as_int(), rows[0][1].as_int()),
            (Some(200), Some(200))
        );
        drop(reader);

        let held = logged_transactions(&*store).await?;
        assert!(held > 8, "the reader held checkpoints back: {held}");
        for _ in 0..30 {
            writer.execute("UPDATE t SET n = n + 1").await?;
        }
        let caught_up = logged_transactions(&*store).await?;
        assert!(
            caught_up <= 8,
            "checkpoints catch up: {held} then {caught_up}"
        );
        let rows = writer.query("SELECT sum(n) FROM t").await?;
        assert_eq!(rows[0][0].as_int(), Some(200 * 41));
        Ok(())
    })
}

/// Two sessions of one owner that both write: the second commit would land at
/// the log position the first already used, so it is refused.
#[test]
fn a_second_concurrent_writer_is_refused() -> Result<()> {
    futures::executor::block_on(async {
        for durability in [Durability::Save, Durability::Commit] {
            let store = Arc::new(MemoryKv::default());
            let namespace = fresh_namespace();
            let opts = || StorageOptions {
                durability,
                ..options(Some(namespace.clone()))
            };
            let mut db = Session::open_with_options(store.clone(), opts()).await?;
            db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
                .await?;
            db.save().await?;
            let mut first = Session::open_with_options(store.clone(), opts()).await?;
            let mut second = Session::open_with_options(store.clone(), opts()).await?;
            first.execute("INSERT INTO t VALUES(1, 0)").await?;
            let refused = match durability {
                Durability::Save => {
                    first.save().await?;
                    second.execute("INSERT INTO t VALUES(2, 0)").await?;
                    second.save().await.map(|_| ())
                }
                Durability::Commit => second.execute("INSERT INTO t VALUES(2, 0)").await,
            };
            assert!(refused.unwrap_err().to_string().contains("another session"));
            let mut db = Session::open_with_options(store.clone(), opts()).await?;
            assert_eq!(count(&mut db).await?, 1, "{durability:?}");
        }
        Ok(())
    })
}

/// An engine parked while another session goes on committing holds no
/// reader position: checkpoints keep the log short.
#[test]
fn a_parked_engine_does_not_hold_back_checkpoints() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let namespace = fresh_namespace();
        let opts = || options(Some(namespace.clone()));
        let mut db = Session::open_with_options(store.clone(), opts()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        drop(db);
        let mut idle = Session::open_with_options(store.clone(), opts()).await?;
        let mut writer = Session::open_with_options(store.clone(), opts()).await?;
        idle.query("SELECT count(*) FROM t").await?;
        drop(idle); // parks: nothing was committed since it opened
        for i in 0..30 {
            writer
                .execute(&format!("INSERT INTO t VALUES({i}, 0)"))
                .await?;
        }
        let logs = store.keys().iter().filter(|k| k.contains("/log/")).count();
        assert!(logs <= 8, "{logs} log segments");
        Ok(())
    })
}

#[test]
fn the_version_grows_with_each_commit_that_changes_or_records() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        db.execute("PRAGMA foreign_keys=ON").await?;
        db.execute(
            "CREATE TABLE p(id INTEGER PRIMARY KEY);
             CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)",
        )
        .await?;
        let v0 = db.version();
        // A commit that changes nothing is not logged...
        db.execute("BEGIN").await?;
        db.execute("UPDATE p SET id = 2 WHERE id = 1").await?;
        db.commit(false).await?;
        assert_eq!(db.version(), v0);
        // ...unless recorded.
        db.execute("BEGIN").await?;
        db.query("SELECT * FROM p").await?;
        db.commit(true).await?;
        let v1 = db.version();
        assert!(v1 > v0);
        // A rollback, or a COMMIT that fails, logs nothing.
        db.execute("BEGIN").await?;
        db.execute("ROLLBACK").await?;
        db.execute("BEGIN").await?;
        db.execute("INSERT INTO c VALUES(7)").await?;
        assert!(db.commit(true).await.is_err());
        db.execute("ROLLBACK").await?;
        assert_eq!(db.version(), v1);
        // The next commit is not recorded unless asked.
        db.execute("SELECT 1").await?;
        assert_eq!(db.version(), v1);
        assert!(db.is_usable());
        drop(db);
        store.crash(&[]);

        // The recorded commit is durable: a later open is at its version.
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(db.version(), v1);
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        Ok(())
    })
}

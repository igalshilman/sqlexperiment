//! `Durability::Commit`: every SQL commit is durable when its statement returns.
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions,
    simulation::{Effect, Failpoint, FaultKv, LatencyKv, MemoryKv, Operation, fresh_namespace},
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
            store.stats().kind("log").puts,
            1,
            "one commit per statement"
        );
        db.execute("INSERT INTO t VALUES(1, 0)").await?;
        assert_eq!(store.stats().kind("log").puts, 2);
        db.query("SELECT * FROM t").await?;
        assert_eq!(store.stats().kind("log").puts, 2, "reads commit nothing");
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
        let before = store.stats().kind("log").puts;
        let id = db
            .transaction(async |db| {
                db.execute("INSERT INTO t VALUES(1, 5)").await?;
                db.execute("INSERT INTO t VALUES(2, -5)").await?;
                Ok(2)
            })
            .await?;
        assert_eq!(id, 2);
        assert_eq!(
            store.stats().kind("log").puts,
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
        assert_eq!(store.stats().kind("log").puts, before + 1, "rolled back");
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

/// With an owner cache, a write transaction costs one round trip, including
/// the checkpoint work that rides along with it.
#[test]
fn a_commit_is_one_round_trip_with_a_cache() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let mut db = Session::open_with_options(store.clone(), options(Some(cache))).await?;
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
            for effect in [Effect::None, Effect::Visible, Effect::After] {
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
    let store = Arc::new(FaultKv::new(MemoryKv::default(), nth as u64));
    let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)")
        .await?;
    let _signal = store.arm(Failpoint {
        operation,
        key_prefix: "v12/".into(),
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
    store.inner.crash(&[]);

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
    let mut db = Session::open_with_options(store, options(None)).await?;
    assert_eq!(count(&mut db).await?, survived + 1);
    Ok(reached)
}

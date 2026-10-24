//! `Durability::Commit`: every SQL commit is durable when its statement returns.
use crate::host::StateSetup;
use crate::host::{Db, Durability, Setup};
use bytes::Bytes;
use state::simulation::Namespace;
use state::{
    Result, StateOptions,
    simulation::{
        Effect, Failpoint, FaultKv, LatencyKv, MemoryKv, MonitorKv, Operation, fresh_namespace,
    },
};
use std::sync::Arc;

fn options(cache: Option<Namespace>) -> Setup {
    Setup(
        StateSetup {
            options: StateOptions {
                ..StateOptions::default()
            },
            cache,
        },
        Durability::Commit,
    )
}

async fn count(db: &mut Db) -> Result<i64> {
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
        let mut db = Db::open(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        assert_eq!(
            store.stats().kind("root").puts,
            1,
            "one flush per statement"
        );
        db.execute("INSERT INTO t VALUES(1, 0)").await?;
        assert_eq!(store.stats().kind("root").puts, 2);
        db.query("SELECT * FROM t").await?;
        assert_eq!(store.stats().kind("root").puts, 2, "reads flush nothing");
        drop(db);
        store.crash();

        let mut db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(count(&mut db).await?, 1);
        Ok(())
    })
}

#[test]
fn a_transaction_commits_as_a_whole_or_not_at_all() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store.clone(), options(None)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        let before = store.stats().kind("root").puts;
        let id = db
            .transaction(async |db| {
                db.execute("INSERT INTO t VALUES(1, 5)").await?;
                db.execute("INSERT INTO t VALUES(2, -5)").await?;
                Ok(2)
            })
            .await?;
        assert_eq!(id, 2);
        assert_eq!(store.stats().kind("root").puts, before + 1, "one flush");

        let failed: Result<()> = db
            .transaction(async |db| {
                db.execute("INSERT INTO t VALUES(3, 7)").await?;
                Err("application error".into())
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(store.stats().kind("root").puts, before + 1, "rolled back");
        assert_eq!(count(&mut db).await?, 2, "the session continues");

        // An open transaction is all that dropping a session loses.
        db.execute("BEGIN").await?;
        db.execute("INSERT INTO t VALUES(4, 1)").await?;
        drop(db);
        store.crash();
        let mut db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(count(&mut db).await?, 2);
        Ok(())
    })
}

/// A long session crosses rebases, garbage collection and the cache's
/// claims, reading its own writes throughout.
#[test]
fn a_long_session_collects_garbage_and_reads_its_writes() -> Result<()> {
    futures::executor::block_on(async {
        for cache in [None, Some(fresh_namespace())] {
            let store = Arc::new(MemoryKv::default());
            let mut db = Db::open(store.clone(), options(cache.clone())).await?;
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
            let blob = store.stats().kind("blob");
            assert!(
                blob.deletes > 0 && store.blob_count() < 80,
                "garbage is collected as the session goes: {} blobs",
                store.blob_count()
            );
            drop(db);
            store.crash();

            let mut db = Db::open(store.clone(), options(cache)).await?;
            assert_eq!(count(&mut db).await?, 120);
            assert_eq!(
                db.query("PRAGMA integrity_check").await?[0][0].to_string(),
                "ok"
            );
        }
        Ok(())
    })
}

/// With an owner cache, a write transaction costs one round trip: its sync.
#[test]
fn a_flush_is_one_round_trip_with_a_cache() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let options = options(Some(cache)).with(|o| StateOptions {
            garbage_after: 32,
            inline_record_bytes: usize::MAX,
            ..o
        });
        let mut db = Db::open(store.clone(), options).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        for i in 0..20 {
            store.reset();
            db.execute(&format!("INSERT INTO t VALUES({i}, 0)")).await?;
            assert_eq!(store.round_trips(), 1, "flush {i}");
        }
        store.reset();
        assert_eq!(count(&mut db).await?, 20);
        assert_eq!(store.round_trips(), 0, "reads are served locally");
        Ok(())
    })
}

/// Crash a flush-per-transaction run at every sync, keeping each prefix of
/// the writes the sync would have stored. Every transaction acknowledged
/// before the crash survives, the one it interrupted may or may not, and
/// nothing else appears.
#[test]
fn every_acknowledged_commit_survives_a_crash_at_any_write() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for cached in [false, true] {
            for nth in 1.. {
                let Some(prefixes) = run_case(cached, nth).await? else {
                    break;
                };
                cases += prefixes;
            }
        }
        println!("commit campaign: {cases} cases");
        assert!(cases > 100, "{cases} cases");
        Ok(())
    })
}

/// Crash at the `nth` sync, keeping each prefix of its writes in turn: the
/// number of prefixes, or `None` if the run made fewer syncs.
async fn run_case(cached: bool, nth: usize) -> Result<Option<usize>> {
    let cache = cached.then(fresh_namespace);
    // Rebases, blob records and garbage collection happen within the run.
    let options = |cache| {
        options(cache).with(|o| StateOptions {
            garbage_after: 8,
            inline_record_bytes: 512,
            ..o
        })
    };
    let store = Arc::new(MonitorKv::new(FaultKv::new(
        MemoryKv::default(),
        nth as u64,
    )));
    let mut db = Db::open(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)")
        .await?;
    let _signal = store.arm(Failpoint {
        operation: Operation::Sync,
        key_prefix: String::new(),
        nth,
        effect: Effect::None,
        cancel: false,
    });
    let mut acknowledged = 0;
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
            Err(_) => break,
        }
    }
    drop(db);
    store.disarm();
    if !store.events().iter().any(|event| event.faulted) {
        return Ok(None);
    }
    assert_eq!(store.violations(), Vec::<String>::new(), "sync #{nth}");
    let writes = store.inner.inner.unsynced().len();
    for kept in 0..=writes {
        let memory = store.inner.inner.fork();
        memory.crash_keeping(kept);
        let store = Arc::new(MonitorKv::seeded(memory.clone(), memory.values()));
        // Reopen through the same owner cache, as a restarted handler would:
        // only to read, as the cache follows one history, and the forks are
        // each another.
        let mut db = Db::open(store.clone(), options(cache.clone())).await?;
        let survived = count(&mut db).await?;
        assert!(
            survived == acknowledged || survived == acknowledged + 2,
            "sync #{nth}, {kept} of {writes} writes kept, cached={cached}: \
             {survived} rows, {acknowledged} acknowledged"
        );
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        drop(db);
        // The namespace stays writable.
        let mut db = Db::open(store.clone(), options(None)).await?;
        db.execute("INSERT INTO t VALUES(1000, 0, 'y')").await?;
        drop(db);
        let mut db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(count(&mut db).await?, survived + 1);
        assert_eq!(
            store.violations(),
            Vec::<String>::new(),
            "sync #{nth}, {kept} kept"
        );
    }
    Ok(Some(writes + 1))
}

/// A string of several statements runs all of them, each under the same rules
/// as on its own, and returns the rows of the last.
#[test]
fn every_statement_of_a_string_runs() -> Result<()> {
    futures::executor::block_on(async {
        for durability in [Durability::Save, Durability::Commit] {
            let store = Arc::new(MemoryKv::default());
            let options = (
                StateOptions {
                    ..StateOptions::default()
                },
                durability,
            );
            let mut db = Db::open(store.clone(), options.clone()).await?;
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
            store.crash();
            let mut db = Db::open(store, options).await?;
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
            let mut db = Db::open(store.clone(), options(None)).await?;
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
            store.crash();
            let mut db = Db::open(store.clone(), options(None)).await?;
            assert_eq!(count(&mut db).await?, 0, "{end}");
        }

        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store, options(None)).await?;
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
        let mut db = Db::open(store.clone(), options(None)).await?;
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

/// A session that only reads, open while another flushes, keeps reading the
/// state it opened at: the blobs its root names stay for `garbage_after`
/// flushes.
#[test]
fn an_overlapping_reader_keeps_its_view() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let namespace = fresh_namespace();
        let opts = || {
            options(Some(namespace.clone())).with(|o| StateOptions {
                garbage_after: 16,
                prefetch_pages: 0,
                ..o
            })
        };
        let mut db = Db::open(store.clone(), opts()).await?;
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

        let mut reader = Db::open(store.clone(), opts()).await?;
        reader.query("SELECT count(*) FROM a").await?;
        let mut writer = Db::open(store.clone(), opts()).await?;
        for _ in 0..10 {
            writer.execute("UPDATE t SET n = n + 1").await?;
        }
        let rows = reader.query("SELECT count(*), sum(n) FROM t").await?;
        assert_eq!(
            (rows[0][0].as_int(), rows[0][1].as_int()),
            (Some(200), Some(200))
        );
        drop(reader);
        let rows = writer.query("SELECT sum(n) FROM t").await?;
        assert_eq!(rows[0][0].as_int(), Some(200 * 11));
        Ok(())
    })
}

/// Two sessions of one owner that both write: the second flush would replace
/// the root the first wrote, so it is refused.
#[test]
fn a_second_concurrent_writer_is_refused() -> Result<()> {
    futures::executor::block_on(async {
        for durability in [Durability::Save, Durability::Commit] {
            let store = Arc::new(MemoryKv::default());
            let namespace = fresh_namespace();
            let opts = || options(Some(namespace.clone())).mode(durability);
            let mut db = Db::open(store.clone(), opts()).await?;
            db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
                .await?;
            db.save().await?;
            let mut first = Db::open(store.clone(), opts()).await?;
            let mut second = Db::open(store.clone(), opts()).await?;
            first.execute("INSERT INTO t VALUES(1, 0)").await?;
            let refused = match durability {
                Durability::Save => {
                    first.save().await?;
                    second.execute("INSERT INTO t VALUES(2, 0)").await?;
                    second.save().await.map(|_| ())
                }
                Durability::Commit => second.execute("INSERT INTO t VALUES(2, 0)").await,
            };
            assert!(refused.unwrap_err().to_string().contains("another state"));
            let mut db = Db::open(store.clone(), opts()).await?;
            assert_eq!(count(&mut db).await?, 1, "{durability:?}");
        }
        Ok(())
    })
}

#[test]
fn effects_commit_with_their_transaction() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store.clone(), options(None)).await?;
        db.execute("PRAGMA foreign_keys=ON").await?;
        db.execute(
            "CREATE TABLE p(id INTEGER PRIMARY KEY);
             CREATE TABLE c(pid INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)",
        )
        .await?;
        assert!(db.effects().is_empty());
        let v0 = db.version();
        // Effects commit even with a transaction that changes nothing...
        db.execute("BEGIN").await?;
        db.query("SELECT * FROM p").await?;
        db.commit_with(Bytes::from_static(b"sends 1")).await?;
        assert_eq!(db.effects(), &b"sends 1"[..]);
        assert_eq!(db.version(), v0 + 1);
        // ...but the same effects again, without changes, flush nothing.
        db.execute("BEGIN").await?;
        db.commit_with(Bytes::from_static(b"sends 1")).await?;
        assert_eq!(db.version(), v0 + 1);
        // A COMMIT that fails commits neither.
        db.execute("BEGIN").await?;
        db.execute("INSERT INTO c VALUES(7)").await?;
        assert!(
            db.commit_with(Bytes::from_static(b"sends 2"))
                .await
                .is_err()
        );
        db.execute("ROLLBACK").await?;
        assert_eq!(db.effects(), &b"sends 1"[..]);
        // Later flushes without effects keep them.
        for i in 0..10 {
            db.execute(&format!("INSERT INTO p VALUES({i})")).await?;
        }
        assert!(db.is_usable());
        drop(db);
        store.crash();

        let mut db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(db.effects(), &b"sends 1"[..]);
        assert_eq!(db.version(), v0 + 11);
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        // An empty transaction clears them.
        db.execute("BEGIN").await?;
        db.commit_with(Bytes::new()).await?;
        drop(db);
        let db = Db::open(store.clone(), options(None)).await?;
        assert!(db.effects().is_empty());
        Ok(())
    })
}

/// Records under the default durability: set in the turn's mutation, unchanged
/// until the save makes them durable, discarded with a dropped session. A
/// large one is a blob of its own, which the flush that replaces it lists as
/// garbage, for the next one to delete.
#[test]
fn records_flush_at_save() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = || StateOptions {
            garbage_after: 1,
            ..StateOptions::default()
        };
        let mut db = Db::open(store.clone(), options()).await?;
        let large = db.state().record("large");
        assert!(large.get().is_empty());
        db.mutation().set(&large, Bytes::from_static(b"one"))?;
        assert!(large.get().is_empty(), "unchanged until the flush");
        drop(db); // discarded, with its mutation
        let mut db = Db::open(store.clone(), options()).await?;
        assert!(db.state().record("large").get().is_empty());
        db.execute("CREATE TABLE t(n)").await?;
        let (large, small) = (db.state().record("large"), db.state().record("small"));
        db.mutation().set(&large, Bytes::from(vec![7; 5000]))?;
        db.mutation().set(&small, Bytes::from_static(b"s"))?;
        db.save().await?;
        let with_large = store.blob_count();

        let mut db = Db::open(store.clone(), options()).await?;
        assert_eq!(db.state().record("large").get(), vec![7; 5000]);
        assert_eq!(db.state().record("small").get(), &b"s"[..]);
        let large = db.state().record("large");
        db.mutation().set(&large, Bytes::from_static(b"two"))?;
        db.save().await?;
        // The replaced value is garbage, which the next flush deletes.
        let mut db = Db::open(store.clone(), options()).await?;
        assert_eq!(db.state().record("large").get(), &b"two"[..]);
        let small = db.state().record("small");
        db.mutation().set(&small, Bytes::new())?;
        db.save().await?;
        assert_eq!(store.blob_count(), with_large - 1, "the large value went");
        let db = Db::open(store.clone(), options()).await?;
        assert!(db.state().record("small").get().is_empty());
        assert_eq!(db.state().record("large").get(), &b"two"[..]);
        Ok(())
    })
}

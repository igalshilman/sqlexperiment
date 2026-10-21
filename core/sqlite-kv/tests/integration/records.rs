//! The host's records: named values a turn commits with its SQL, in one
//! commit record (`Session::seal`).
use bytes::Bytes;
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use std::sync::Arc;

fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        durability: Durability::Commit,
        ..StorageOptions::default()
    }
}

/// Turn `i`'s value: small, past the inline budget so that commits seal, or
/// past it alone, so that it is stored first.
fn value(i: usize) -> Bytes {
    let len = [100, 1500, 9000][i % 3];
    Bytes::from((0..len).map(|n| (n * 31 + i) as u8).collect::<Vec<_>>())
}

/// The record `turn` after turns `0..turns`: the last one's value.
fn expected(turns: usize) -> Bytes {
    turns.checked_sub(1).map(value).unwrap_or_default()
}

/// A turn: a row (every other turn, in a transaction; the others open none),
/// its number as the record `effects`, its value as the record `turn` (small,
/// sealed, or stored), and every eleventh empties the record `state`, every
/// third sets it to its value.
async fn turn(db: &mut Session, i: usize) -> Result<()> {
    if i.is_multiple_of(2) {
        db.execute("BEGIN").await?;
        db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    }
    db.record("effects").set(Bytes::from(format!("turn {i}")));
    db.record("turn").set(value(i));
    if i % 11 == 10 {
        db.record("state").set(Bytes::new());
    } else if i.is_multiple_of(3) {
        db.record("state").set(value(i));
    }
    db.seal().await.map(drop)
}

/// The record `state` after turns `0..turns`.
fn state(turns: usize) -> Bytes {
    (0..turns)
        .rev()
        .find(|i| i.is_multiple_of(3) || i % 11 == 10)
        .filter(|i| i % 11 != 10)
        .map(value)
        .unwrap_or_default()
}

async fn rows(db: &mut Session) -> Result<i64> {
    let rows = db.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn a_turn_commits_its_records_with_its_sql() -> Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(MonitorKv::new(MemoryKv::default()));
            let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
            db.execute("CREATE TABLE t(n INTEGER)").await?;
            for i in 0..60 {
                turn(&mut db, i).await?;
                assert_eq!(db.record("turn").get(), expected(i + 1), "turn {i}");
                if i % 7 == 6 {
                    // Another session reads the same turn: the record, and the
                    // stored values it names.
                    drop(db);
                    let mut other =
                        Session::open_with_options(store.clone(), options(None)).await?;
                    assert_eq!(
                        other.record("turn").get(),
                        expected(i + 1),
                        "turn {i}, reopened"
                    );
                    assert_eq!(other.effects(), format!("turn {i}").as_bytes());
                    assert_eq!(other.record("state").get(), state(i + 1));
                    assert_eq!(rows(&mut other).await?, (i / 2 + 1) as i64);
                    drop(other);
                    db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
                }
            }
            // A turn without SQL changes adds no log transaction: checkpoints
            // stay with the SQL.
            assert_eq!(store.violations(), Vec::<String>::new());
            // What no record names is garbage, and the next commit deletes it:
            // a few stored values stay, not one per turn.
            db.execute("BEGIN").await?;
            db.commit_with(Bytes::from_static(b"done")).await?;
            let stored = store
                .inner
                .keys()
                .iter()
                .filter(|k| k.contains("/log/"))
                .count();
            assert!(stored <= 6, "cached={cached}: {stored} stored values");
        }
        Ok(())
    })
}

/// A turn of no SQL, in a transaction or without one, is one commit, and adds
/// nothing to the log.
#[test]
fn turns_without_sql_leave_the_log_alone() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db =
            Session::open_with_options(store.clone(), options(Some(fresh_namespace()))).await?;
        db.execute("CREATE TABLE t(n INTEGER)").await?;
        let version = db.version();
        for i in 0..100 {
            if i % 3 == 0 {
                db.execute("BEGIN").await?;
            }
            db.record("turn").set(Bytes::from(vec![i as u8; 64]));
            db.seal().await?;
        }
        assert_eq!(db.version(), version + 100);
        // Without SQL nothing checkpointed: no page keys beyond the first
        // commit's.
        let pages = |store: &MemoryKv| store.keys().iter().filter(|k| k.contains("/page/")).count();
        assert_eq!(pages(&store), 0);
        drop(db);
        let db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(db.record("turn").get(), vec![99u8; 64]);
        Ok(())
    })
}

/// Crash a run of turns at every sync, keeping each prefix of the writes the
/// sync would have stored. The records and the rows are those of
/// the last acknowledged turn, or of the interrupted one, together.
#[test]
fn every_acknowledged_turn_survives_a_crash_at_any_write() -> Result<()> {
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
        println!("turn campaign: {cases} cases");
        assert!(cases > 100, "{cases} cases");
        Ok(())
    })
}

/// Crash at the `nth` sync, keeping each prefix of its writes in turn: the
/// number of prefixes, or `None` if the run made fewer syncs.
async fn run_case(cached: bool, nth: usize) -> Result<Option<usize>> {
    let cache = cached.then(fresh_namespace);
    // Checkpoints happen within the run.
    let options = |cache| StorageOptions {
        checkpoint_after: 4,
        ..options(cache)
    };
    let store = Arc::new(MonitorKv::new(FaultKv::new(
        MemoryKv::default(),
        nth as u64,
    )));
    let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(n INTEGER)").await?;
    let _signal = store.arm(Failpoint {
        operation: Operation::Sync,
        key_prefix: String::new(),
        nth,
        effect: Effect::None,
        cancel: false,
    });
    let mut acknowledged = 0;
    for i in 0..40 {
        match turn(&mut db, i).await {
            Ok(()) => acknowledged = i + 1,
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
        let case = format!("sync #{nth}, {kept} of {writes} writes kept, cached={cached}");
        // Reopen through the same owner cache, as a restarted handler would:
        // only to read, as the cache follows one history, and the forks are
        // each another.
        let db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
        let turns = match db.record("turn").get() == expected(acknowledged) {
            true => acknowledged,
            false => {
                assert_eq!(
                    db.record("turn").get(),
                    expected(acknowledged + 1),
                    "{case}"
                );
                acknowledged + 1
            }
        };
        drop(db);
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        if turns > 0 {
            assert_eq!(
                db.effects(),
                format!("turn {}", turns - 1).as_bytes(),
                "{case}"
            );
        }
        assert_eq!(db.record("state").get(), state(turns), "{case}");
        assert_eq!(rows(&mut db).await?, turns.div_ceil(2) as i64, "{case}");
        // The namespace takes turns again.
        turn(&mut db, turns).await?;
        drop(db);
        let db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(db.record("turn").get(), expected(turns + 1), "{case}");
        assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
    }
    Ok(Some(writes + 1))
}

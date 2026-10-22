//! The host's records: named values a turn flushes with its SQL, in one
//! commit record.
use crate::host::{Db, Durability, Setup};
use bytes::Bytes;
use state::{
    NamespaceCache, Result, StorageOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use std::sync::Arc;

fn options(cache: Option<NamespaceCache>) -> Setup {
    Setup(
        StorageOptions {
            cache,
            ..StorageOptions::default()
        },
        Durability::Commit,
    )
}

/// Turn `i`'s value: small, past the inline budget so that flushes seal, or
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
async fn turn(db: &mut Db, i: usize) -> Result<()> {
    if i.is_multiple_of(2) {
        db.execute("BEGIN").await?;
        db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    }
    let state = db.state();
    let wal = db.wal();
    wal.set(&state.record("effects"), Bytes::from(format!("turn {i}")))?;
    wal.set(&state.record("turn"), value(i))?;
    if i % 11 == 10 {
        wal.set(&state.record("progress"), Bytes::new())?;
    } else if i.is_multiple_of(3) {
        wal.set(&state.record("progress"), value(i))?;
    }
    db.flush().await.map(drop)
}

/// The record `state` after turns `0..turns`.
fn progress(turns: usize) -> Bytes {
    (0..turns)
        .rev()
        .find(|i| i.is_multiple_of(3) || i % 11 == 10)
        .filter(|i| i % 11 != 10)
        .map(value)
        .unwrap_or_default()
}

async fn rows(db: &mut Db) -> Result<i64> {
    let rows = db.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn a_turn_flushes_its_records_with_its_sql() -> Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(MonitorKv::new(MemoryKv::default()));
            let mut db = Db::open(store.clone(), options(cache.clone())).await?;
            db.execute("CREATE TABLE t(n INTEGER)").await?;
            for i in 0..60 {
                turn(&mut db, i).await?;
                assert_eq!(db.state().record("turn").get(), expected(i + 1), "turn {i}");
                if i % 7 == 6 {
                    // Another session reads the same turn: the record, and the
                    // stored values it names.
                    drop(db);
                    let mut other = Db::open(store.clone(), options(None)).await?;
                    assert_eq!(
                        other.state().record("turn").get(),
                        expected(i + 1),
                        "turn {i}, reopened"
                    );
                    assert_eq!(other.effects(), format!("turn {i}").as_bytes());
                    assert_eq!(other.state().record("progress").get(), progress(i + 1));
                    assert_eq!(rows(&mut other).await?, (i / 2 + 1) as i64);
                    drop(other);
                    db = Db::open(store.clone(), options(cache.clone())).await?;
                }
            }
            // A turn without SQL changes adds no log transaction: checkpoints
            // stay with the SQL.
            assert_eq!(store.violations(), Vec::<String>::new());
            // What no record names is garbage, and the next flush deletes it:
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

/// A turn of no SQL, in a transaction or without one, is one flush, and adds
/// nothing to the log.
#[test]
fn turns_without_sql_leave_the_log_alone() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store.clone(), options(Some(fresh_namespace()))).await?;
        db.execute("CREATE TABLE t(n INTEGER)").await?;
        let version = db.version();
        for i in 0..100 {
            if i % 3 == 0 {
                db.execute("BEGIN").await?;
            }
            let turn = db.state().record("turn");
            db.wal().set(&turn, Bytes::from(vec![i as u8; 64]))?;
            db.flush().await?;
        }
        assert_eq!(db.version(), version + 100);
        // Without SQL nothing checkpointed: no page keys beyond the first
        // flush's.
        let pages = |store: &MemoryKv| store.keys().iter().filter(|k| k.contains("/page/")).count();
        assert_eq!(pages(&store), 0);
        drop(db);
        let db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(db.state().record("turn").get(), vec![99u8; 64]);
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
        for durability in [Durability::Commit, Durability::Save] {
            for cached in [false, true] {
                for nth in 1.. {
                    let Some(prefixes) = run_case(durability, cached, nth).await? else {
                        break;
                    };
                    cases += prefixes;
                }
            }
        }
        println!("turn campaign: {cases} cases");
        assert!(cases > 100, "{cases} cases");
        Ok(())
    })
}

/// Crash at the `nth` sync, keeping each prefix of its writes in turn: the
/// number of prefixes, or `None` if the run made fewer syncs.
async fn run_case(durability: Durability, cached: bool, nth: usize) -> Result<Option<usize>> {
    let cache = cached.then(fresh_namespace);
    // Checkpoints happen within the run.
    let options = |cache| {
        options(cache)
            .with(|o| StorageOptions {
                checkpoint_after: 4,
                ..o
            })
            .mode(durability)
    };
    let store = Arc::new(MonitorKv::new(FaultKv::new(
        MemoryKv::default(),
        nth as u64,
    )));
    let mut db = Db::open(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(n INTEGER)").await?;
    db.flush().await?;
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
        let case =
            format!("{durability:?}, sync #{nth}, {kept} of {writes} writes kept, cached={cached}");
        // Reopen through the same owner cache, as a restarted handler would:
        // only to read, as the cache follows one history, and the forks are
        // each another.
        let db = Db::open(store.clone(), options(cache.clone())).await?;
        let turns = match db.state().record("turn").get() == expected(acknowledged) {
            true => acknowledged,
            false => {
                assert_eq!(
                    db.state().record("turn").get(),
                    expected(acknowledged + 1),
                    "{case}"
                );
                acknowledged + 1
            }
        };
        drop(db);
        let mut db = Db::open(store.clone(), options(None)).await?;
        if turns > 0 {
            assert_eq!(
                db.effects(),
                format!("turn {}", turns - 1).as_bytes(),
                "{case}"
            );
        }
        assert_eq!(
            db.state().record("progress").get(),
            progress(turns),
            "{case}"
        );
        assert_eq!(rows(&mut db).await?, turns.div_ceil(2) as i64, "{case}");
        // The namespace takes turns again.
        turn(&mut db, turns).await?;
        drop(db);
        let db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(
            db.state().record("turn").get(),
            expected(turns + 1),
            "{case}"
        );
        assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
    }
    Ok(Some(writes + 1))
}

/// A scoped state names its records and files under its prefix, and
/// flushes with the session.
#[test]
fn a_scoped_state_is_part_of_the_session_s() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store.clone(), options(None)).await?;
        let fs = db.state().scoped("fs/");
        db.wal()
            .set(&fs.record("manifest"), Bytes::from_static(b"m"))?;
        db.wal().write(
            &fs.scoped("blobs/").file("a")?,
            0,
            Some(vec![7; 4096].into()),
        )?;
        db.flush().await?;
        drop(db);
        let db = Db::open(store, options(None)).await?;
        let state = db.state();
        assert_eq!(state.record("fs/manifest").get(), &b"m"[..]);
        assert_eq!(state.scoped("fs/").record("manifest").get(), &b"m"[..]);
        let file = state.file("fs/blobs/a")?;
        assert_eq!(file.len(), 4096);
        assert_eq!(file.read(0).await?.unwrap()[0], 7);
        Ok(())
    })
}

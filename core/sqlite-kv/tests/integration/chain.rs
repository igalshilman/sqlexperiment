//! The host's chain: values a turn commits with its SQL and effects, in one
//! record (`Session::commit_turn`).
use bytes::Bytes;
use sqlite_kv::{
    ChainUpdate, Durability, NamespaceCache, Result, Session, StorageOptions,
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

/// Turn `i`'s update: every fifth restarts the chain.
fn update(i: usize) -> ChainUpdate {
    match i % 5 {
        4 => ChainUpdate::Restart(value(i)),
        _ => ChainUpdate::Append(value(i)),
    }
}

/// The chain after turns `0..turns`.
fn expected(turns: usize) -> Vec<Bytes> {
    let mut chain = Vec::new();
    for i in 0..turns {
        match update(i) {
            ChainUpdate::Append(value) => chain.push(value),
            ChainUpdate::Restart(value) => chain = vec![value],
        }
    }
    chain
}

/// A turn: a row (every other turn), its state as the effects, its update.
async fn turn(db: &mut Session, i: usize) -> Result<()> {
    db.execute("BEGIN").await?;
    if i.is_multiple_of(2) {
        db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    }
    db.commit_turn(Bytes::from(format!("turn {i}")), update(i))
        .await
}

async fn rows(db: &mut Session) -> Result<i64> {
    let rows = db.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

#[test]
fn a_turn_commits_its_chain_with_its_sql_and_effects() -> Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(MonitorKv::new(MemoryKv::default()));
            let mut db = Session::open_with_options(store.clone(), options(cache.clone())).await?;
            db.execute("CREATE TABLE t(n INTEGER)").await?;
            for i in 0..60 {
                turn(&mut db, i).await?;
                assert_eq!(db.chain(), expected(i + 1), "turn {i}");
                if i % 7 == 6 {
                    // Another session reads the same turn: the record, and the
                    // stored values it names.
                    drop(db);
                    let mut other =
                        Session::open_with_options(store.clone(), options(None)).await?;
                    assert_eq!(other.chain(), expected(i + 1), "turn {i}, reopened");
                    assert_eq!(other.effects(), format!("turn {i}").as_bytes());
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

/// A chain value past the inline budget is stored first; a turn of no SQL
/// is one commit, and adds nothing to the log.
#[test]
fn turns_without_sql_leave_the_log_alone() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db =
            Session::open_with_options(store.clone(), options(Some(fresh_namespace()))).await?;
        db.execute("CREATE TABLE t(n INTEGER)").await?;
        let version = db.version();
        for i in 0..100 {
            db.execute("BEGIN").await?;
            db.commit_turn(
                Bytes::new(),
                ChainUpdate::Append(Bytes::from(vec![i as u8; 64])),
            )
            .await?;
        }
        assert_eq!(db.version(), version + 100);
        // Without SQL nothing checkpointed: no page keys beyond the first
        // commit's.
        let pages = |store: &MemoryKv| store.keys().iter().filter(|k| k.contains("/page/")).count();
        assert_eq!(pages(&store), 0);
        drop(db);
        let db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(db.chain().len(), 100);
        assert_eq!(db.chain()[99], vec![99u8; 64]);
        Ok(())
    })
}

/// Fail every mutation of a run of turns in turn. After a crash, the chain,
/// the effects and the rows are those of the last acknowledged turn, or of the
/// failed one, together.
#[test]
fn every_acknowledged_turn_survives_any_single_failure() -> Result<()> {
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
        println!("turn campaign: {cases} cases");
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
    db.execute("CREATE TABLE t(n INTEGER)").await?;
    let _signal = store.arm(Failpoint {
        operation,
        key_prefix: "v14/".into(),
        nth,
        effect,
        cancel: false,
    });
    let mut acknowledged = 0;
    let mut failed = false;
    for i in 0..40 {
        match turn(&mut db, i).await {
            Ok(()) => acknowledged = i + 1,
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

    let case = format!("{effect:?} {operation:?} #{nth} cached={cached}");
    let mut db = Session::open_with_options(store.clone(), options(cache)).await?;
    let turns = match db.chain() == expected(acknowledged) {
        true => acknowledged,
        false => {
            assert!(failed, "{case}: the chain of no turn");
            assert_eq!(db.chain(), expected(acknowledged + 1), "{case}");
            acknowledged + 1
        }
    };
    if turns > 0 {
        assert_eq!(
            db.effects(),
            format!("turn {}", turns - 1).as_bytes(),
            "{case}"
        );
    }
    assert_eq!(rows(&mut db).await?, turns.div_ceil(2) as i64, "{case}");
    // The namespace takes turns again.
    turn(&mut db, turns).await?;
    drop(db);
    let db = Session::open_with_options(store.clone(), options(None)).await?;
    assert_eq!(db.chain(), expected(turns + 1), "{case}");
    assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
    Ok(reached)
}

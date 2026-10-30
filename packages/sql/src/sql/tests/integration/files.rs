//! Paged files beside the SQL database (`State::file`): written with the
//! tables, in one head, and kept like the database's own pages.
use super::host::StateSetup;
use super::host::{Db, Durability, Setup};
use crate::sql::Sql;
use crate::state::simulation::Namespace;
use crate::state::{
    Result, StateOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use bytes::Bytes;
use std::sync::Arc;

fn page(byte: u8) -> Bytes {
    Bytes::from(vec![byte; 4096])
}

fn options(cache: Option<Namespace>) -> Setup {
    Setup(
        StateSetup {
            options: StateOptions {
                // Garbage is deleted within a run of turns.
                checkpoint_entries: 4,
                ..StateOptions::default()
            },
            cache,
            packs: None,
        },
        Durability::Commit,
    )
}

/// Turn `i`: a row, page `i % 5` of `a` and page `i % 3` of `b` hold `i + 1`
/// (never zeros, which read as a hole), as does page `i % 4` of `c`, and
/// every seventh turn empties `b`.
async fn turn(db: &mut Db, i: usize) -> Result<()> {
    db.execute("BEGIN").await?;
    db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    let state = db.state();
    let mutation = db.mutation();
    mutation.write(&state.file("a")?, (i % 5) as u64, Some(page(i as u8 + 1)))?;
    mutation.write(&state.file("c")?, (i % 4) as u64, Some(page(i as u8 + 1)))?;
    let b = state.file("b")?;
    match i % 7 {
        6 => mutation.set_len(&b, 0)?,
        _ => mutation.write(&b, (i % 3) as u64, Some(page(i as u8 + 1)))?,
    }
    db.flush().await.map(drop)
}

/// File `name`'s pages after turns `0..turns`: each page's first byte,
/// `None` for a hole.
fn expected(name: &str, turns: usize) -> Vec<Option<u8>> {
    let mut pages: Vec<Option<u8>> = Vec::new();
    for i in 0..turns {
        let at = match name {
            "a" => i % 5,
            "c" => i % 4,
            _ if i % 7 == 6 => {
                pages.clear();
                continue;
            }
            _ => i % 3,
        };
        if pages.len() <= at {
            pages.resize(at + 1, None);
        }
        pages[at] = Some(i as u8 + 1);
    }
    pages
}

/// File `name`'s pages as `db` holds them (each one's first byte).
async fn pages(db: &Sql, name: &str) -> Result<Vec<Option<u8>>> {
    let file = db.state().file(name)?;
    let mut pages = Vec::new();
    for n in 0..file.len() / 4096 {
        pages.push(file.read(n).await?.map(|page| page[0]));
    }
    Ok(pages)
}

async fn rows(db: &mut Db) -> Result<usize> {
    let rows = db.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap() as usize)
}

/// Crash a run of turns at every sync, keeping each prefix of the writes the
/// sync would have stored: the files and the rows are those of the last
/// acknowledged turn, or of the interrupted one, together.
#[test]
fn two_files_and_the_tables_survive_a_crash_at_any_write_together() -> Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for cached in [false, true] {
            for nth in 1.. {
                let Some(prefixes) = crash_case(cached, nth).await? else {
                    break;
                };
                cases += prefixes;
            }
        }
        assert!(cases > 100, "{cases} cases");
        Ok(())
    })
}

async fn crash_case(cached: bool, nth: usize) -> Result<Option<usize>> {
    let cache = cached.then(fresh_namespace);
    let store = Arc::new(MonitorKv::new(FaultKv::new(
        MemoryKv::default(),
        nth as u64,
    )));
    let mut db = Db::open(store.clone(), options(cache.clone())).await?;
    db.execute("CREATE TABLE t(n INTEGER)").await?;
    let _signal = store.arm(Failpoint {
        operation: Operation::Sync,
        key_prefix: String::new(),
        nth,
        effect: Effect::None,
        cancel: false,
    });
    let mut acknowledged = 0;
    for i in 0..24 {
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
        let mut db = Db::open(store.clone(), options(None)).await?;
        let turns = rows(&mut db).await?;
        assert!(
            turns == acknowledged || turns == acknowledged + 1,
            "{case}: {turns} turns"
        );
        for name in ["a", "b", "c"] {
            assert_eq!(
                pages(&db, name).await?,
                expected(name, turns),
                "{case}, {name}"
            );
        }
        // The namespace takes turns again.
        turn(&mut db, turns).await?;
        drop(db);
        let db = Db::open(store.clone(), options(None)).await?;
        assert_eq!(pages(&db, "a").await?, expected("a", turns + 1), "{case}");
        assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
    }
    Ok(Some(writes + 1))
}

/// Garbage goes at the next save.
fn collecting(cache: Option<Namespace>) -> StateSetup {
    StateSetup {
        options: StateOptions {
            checkpoint_entries: 1,
            ..StateOptions::default()
        },
        cache,
        packs: None,
    }
}

/// A file emptied, then regrown, reads zeros where it held pages; the packs
/// that held them go two checkpoints later.
#[test]
fn an_emptied_file_frees_its_packs() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()));
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        for n in 0..6 {
            db.mutation().write(&file, n, Some(page(n as u8 + 1)))?;
        }
        assert!(file.is_empty(), "unchanged until the flush");
        db.save().await?;

        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.len(), 6 * 4096);
        assert_eq!(file.read(5).await?, Some(page(6)));
        db.mutation().set_len(&file, 0)?;
        assert_eq!(file.read(5).await?, Some(page(6)), "until the flush");
        db.flush().await?;
        assert_eq!(db.state().collect_garbage().await?, 0, "kept for readers");
        db.save().await?;

        // Regrown, it reads zeros where it held pages.
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        assert!(file.is_empty());
        db.mutation().write(&file, 7, Some(page(9)))?;
        db.flush().await?;
        let mut deleted = 0;
        for n in 0..2u8 {
            let record = db.state().record("n");
            db.mutation().set(&record, vec![n].into())?;
            let report = db.flush().await?;
            deleted += report.checkpoint.map_or(0, |checkpoint| checkpoint.deleted);
        }
        assert!(deleted > 0, "the garbage went");
        assert_eq!(store.inner.packs(), db.state().needed_packs().await?);
        let db = Db::open(store.clone(), StateOptions::default()).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.len(), 8 * 4096);
        assert_eq!(file.read(5).await?, None);
        assert_eq!(file.read(7).await?, Some(page(9)));
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

/// What the owner checkpointed stays in its cache when another writer
/// flushes: a pack never changes, so the owner fetches only the tail.
#[test]
fn a_files_frames_stay_cached_across_other_writers() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let mut db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        for n in 0..40 {
            db.mutation().write(&file, n, Some(page(n as u8 + 1)))?;
        }
        db.save_checkpointed().await?;

        // The owner reads them back from the cache: only the head is read.
        store.reset_stats();
        let db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        for n in 0..40 {
            assert_eq!(file.read(n).await?, Some(page(n as u8 + 1)));
        }
        let stats = store.stats();
        assert_eq!((stats.kind("tail").gets, stats.kind("pack").gets), (0, 0));
        drop(db);

        // Another writer, which does not checkpoint, changes a page.
        let never = StateOptions {
            checkpoint_entries: 0,
            checkpoint_bytes: 0,
            ..StateOptions::default()
        };
        let mut other = Db::open(store.clone(), never).await?;
        let m = other.state().file("m")?;
        other.mutation().write(&m, 3, Some(page(99)))?;
        other.save().await?;
        store.reset_stats();
        let db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.read(3).await?, Some(page(99)));
        assert_eq!(file.read(4).await?, Some(page(5)));
        let stats = store.stats();
        // The new entry, and the checkpointing flush's own, which its
        // checkpoint does not hold.
        assert_eq!((stats.kind("tail").gets, stats.kind("pack").gets), (2, 0));
        Ok(())
    })
}

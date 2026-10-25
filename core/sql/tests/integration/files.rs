//! Paged files beside the SQL database (`State::file`): written with the
//! tables, in one root, and kept like the database's own pages.
use crate::host::StateSetup;
use crate::host::{Db, Durability, Setup};
use bytes::Bytes;
use sql::Sql;
use state::simulation::Namespace;
use state::{
    Base, Result, StateOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use std::sync::Arc;

fn page(byte: u8) -> Bytes {
    Bytes::from(vec![byte; 4096])
}

/// A base of `pages` pages of varied bytes, as a VM's golden image is.
struct Golden {
    id: u64,
    bytes: Vec<u8>,
}

impl Golden {
    fn new(id: u64, pages: usize) -> Arc<Self> {
        let mut state = id.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let bytes = (0..pages * 4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        Arc::new(Self { id, bytes })
    }

    /// Its page `n`, with the first byte set to `byte`: a small change.
    fn changed(&self, n: u64, byte: u8) -> Bytes {
        let mut page = self.page(n).unwrap().to_vec();
        page[0] = byte;
        Bytes::from(page)
    }
}

impl Base for Golden {
    fn id(&self) -> u64 {
        self.id
    }

    fn page(&self, index: u64) -> Option<&[u8]> {
        let at = index as usize * 4096;
        self.bytes.get(at..at + 4096)
    }
}

fn golden() -> Arc<Golden> {
    Golden::new(7, 8)
}

fn options(cache: Option<Namespace>) -> Setup {
    Setup(
        StateSetup {
            options: StateOptions {
                // Garbage is deleted within a run of turns.
                garbage_after: 4,
                ..StateOptions::default()
            },
            cache,
        },
        Durability::Commit,
    )
}

/// Turn `i`: a row, page `i % 5` of `a` and page `i % 3` of `b` hold `i + 1`
/// (never zeros, which read as a hole), every seventh turn empties `b`, and
/// page `i % 4` of `c` (over the golden image) is its golden page with the
/// first byte `i + 1`.
async fn turn(db: &mut Db, i: usize) -> Result<()> {
    db.execute("BEGIN").await?;
    db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    let state = db.state();
    let mutation = db.mutation();
    mutation.write(&state.file("a")?, (i % 5) as u64, Some(page(i as u8 + 1)))?;
    let c = state.file_with_base("c", golden())?;
    mutation.write(
        &c,
        (i % 4) as u64,
        Some(golden().changed((i % 4) as u64, i as u8 + 1)),
    )?;
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
    let file = match name {
        "c" => db.state().file_with_base(name, golden())?,
        _ => db.state().file(name)?,
    };
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
    let store = Arc::new(
        MonitorKv::new(FaultKv::new(MemoryKv::default(), nth as u64)).with_base("c", golden()),
    );
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
        let store =
            Arc::new(MonitorKv::seeded(memory.clone(), memory.values()).with_base("c", golden()));
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
            garbage_after: 1,
            ..StateOptions::default()
        },
        cache,
    }
}

#[test]
fn an_emptied_file_frees_its_blobs_a_sweep_later() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()));
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        for n in 0..6 {
            db.mutation().write(&file, n, Some(page(n as u8 + 1)))?;
        }
        assert!(file.is_empty(), "unchanged until the flush");
        db.save().await?;
        assert_eq!(store.inner.blob_count(), 2, "two chunks of four pages");

        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.len(), 6 * 4096);
        assert_eq!(file.read(5).await?, Some(page(6)));
        db.mutation().set_len(&file, 0)?;
        assert_eq!(file.read(5).await?, Some(page(6)), "until the flush");
        db.flush().await?;
        let swept = db.state().collect_garbage().await?;
        assert_eq!((swept.deleted, swept.dead), (0, 2), "kept for readers");
        db.save().await?;

        // Regrown, it reads zeros where it held pages.
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        assert!(file.is_empty());
        db.mutation().write(&file, 7, Some(page(9)))?;
        db.flush().await?;
        db.state().collect_garbage().await?;
        assert_eq!(store.inner.blob_count(), 1, "the garbage went");
        let db = Db::open(store.clone(), StateOptions::default()).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.len(), 8 * 4096);
        assert_eq!(file.read(5).await?, None);
        assert_eq!(file.read(7).await?, Some(page(9)));
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

#[test]
fn a_files_blobs_stay_cached_across_other_writers() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let mut db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        for n in 0..40 {
            db.mutation().write(&file, n, Some(page(n as u8 + 1)))?;
        }
        db.save().await?;

        // The owner reads them back from the cache.
        store.reset_stats();
        let db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        for n in 0..40 {
            assert_eq!(file.read(n).await?, Some(page(n as u8 + 1)));
        }
        assert_eq!(store.stats().kind("blob").gets, 0);
        drop(db);

        // Another writer changes one chunk: the owner reads only its new
        // blob from the store, as blobs never change.
        let mut other = Db::open(store.clone(), collecting(None)).await?;
        let m = other.state().file("m")?;
        other.mutation().write(&m, 3, Some(page(99)))?;
        other.save().await?;
        store.reset_stats();
        let db = Db::open(store.clone(), collecting(Some(cache.clone()))).await?;
        let file = db.state().file("m")?;
        assert_eq!(file.read(3).await?, Some(page(99)));
        assert_eq!(file.read(4).await?, Some(page(5)));
        assert_eq!(store.stats().kind("blob").gets, 1);
        Ok(())
    })
}

#[test]
fn a_file_over_a_base_reads_it_where_it_has_no_page() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()).with_base("m", golden()));
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file_with_base("m", golden())?;
        db.mutation().set_len(&file, 8 * 4096)?;
        assert_eq!(file.read(3).await?, None, "empty until the flush");
        db.flush().await?;
        assert_eq!(file.read(3).await?.as_deref(), golden().page(3));
        db.mutation()
            .write(&file, 2, Some(golden().changed(2, 1)))?;
        db.mutation().write(&file, 5, Some(page(9)))?;
        // A page written back as its base's is a hole again.
        db.mutation()
            .write(&file, 6, Some(golden().changed(6, 1)))?;
        let golden_6 = Bytes::copy_from_slice(golden().page(6).unwrap());
        db.mutation().write(&file, 6, Some(golden_6))?;
        db.save().await?;

        let db = Db::open(store.clone(), StateOptions::default()).await?;
        let file = db.state().file_with_base("m", golden())?;
        assert_eq!(file.read(2).await?, Some(golden().changed(2, 1)));
        assert_eq!(file.read(3).await?.as_deref(), golden().page(3));
        assert_eq!(file.read(5).await?, Some(page(9)));
        assert_eq!(file.read(6).await?.as_deref(), golden().page(6));
        // Two chunks differ from the base: a delta each.
        assert_eq!(store.inner.blob_count(), 2);
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

#[test]
fn a_small_change_to_a_base_page_costs_little() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        for n in 1..=2 {
            let mut db = Db::open(store.clone(), collecting(None)).await?;
            let file = db.state().file_with_base("m", golden())?;
            db.mutation()
                .write(&file, 4, Some(golden().changed(4, n)))?;
            store.reset_stats();
            db.save().await?;
            // A delta against the base, and the root.
            let written = store.stats().bytes_written;
            assert!(written < 300, "{written} bytes written");
        }
        let db = Db::open(store.clone(), StateOptions::default()).await?;
        let file = db.state().file_with_base("m", golden())?;
        assert_eq!(file.read(4).await?, Some(golden().changed(4, 2)));
        Ok(())
    })
}

#[test]
fn a_file_opens_only_with_its_base_and_empties_without_it() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file_with_base("m", golden())?;
        db.mutation()
            .write(&file, 1, Some(golden().changed(1, 1)))?;
        db.save().await?;

        let mut db = Db::open(store.clone(), StateOptions::default()).await?;
        assert!(db.state().file_with_base("m", Golden::new(8, 8)).is_err());
        assert!(db.state().file("plain")?.read(0).await?.is_none());
        let file = db.state().file("m")?;
        assert!(file.read(1).await.is_err(), "it needs its base");
        let refused = db.mutation().write(&file, 1, Some(page(1))).is_err();
        assert!(refused || db.save().await.is_err());

        // Emptied without its base: no blobs are left.
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let file = db.state().file("m")?;
        db.mutation().write(&file, 1, None)?;
        db.mutation().set_len(&file, 0)?;
        db.flush().await?;
        db.state().collect_garbage().await?;
        let mut db = Db::open(store.clone(), collecting(None)).await?;
        let next = db.state().record("next");
        db.mutation().set(&next, Bytes::from_static(b"save"))?;
        db.flush().await?;
        db.state().collect_garbage().await?;
        assert_eq!(store.blob_count(), 0, "its blobs went a sweep later");
        Ok(())
    })
}

#[test]
fn a_based_file_keeps_its_deltas_while_others_save_without_its_base() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()).with_base("m", golden()));
        let mut db = Db::open(store.clone(), StateOptions::default()).await?;
        let m = db.state().file_with_base("m", golden())?;
        db.mutation().write(&m, 1, Some(golden().changed(1, 5)))?;
        db.save().await?;
        // Saves that never open it leave its delta as it is.
        for n in 0..3 {
            let mut db = Db::open(store.clone(), collecting(None)).await?;
            db.execute(&format!("CREATE TABLE t{n}(n)")).await?;
            db.save().await?;
        }
        let db = Db::open(store.clone(), StateOptions::default()).await?;
        let file = db.state().file_with_base("m", golden())?;
        assert_eq!(file.read(1).await?, Some(golden().changed(1, 5)));
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

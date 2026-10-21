//! Paged files beside the SQL database (`Session::file`): written with the
//! tables, in one commit record, and kept like the database's own pages.
use bytes::Bytes;
use sqlite_kv::{
    Durability, NamespaceCache, Result, Session, StorageOptions,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use std::sync::Arc;

fn page(byte: u8) -> Bytes {
    Bytes::from(vec![byte; 4096])
}

fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        durability: Durability::Commit,
        // Checkpoints happen within a run of turns.
        checkpoint_after: 4,
        ..StorageOptions::default()
    }
}

/// Turn `i`: a row, page `i % 5` of `a` and page `i % 3` of `b` hold `i`,
/// and every seventh turn empties `b`.
async fn turn(db: &mut Session, i: usize) -> Result<()> {
    db.execute("BEGIN").await?;
    db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    db.file("a")?.write((i % 5) as u64, Some(page(i as u8)))?;
    let b = db.file("b")?;
    match i % 7 {
        6 => b.set_len(0)?,
        _ => b.write((i % 3) as u64, Some(page(i as u8)))?,
    }
    db.seal(None).await
}

/// File `name`'s pages after turns `0..turns`: each page's byte, `None` for
/// a hole.
fn expected(name: &str, turns: usize) -> Vec<Option<u8>> {
    let mut pages: Vec<Option<u8>> = Vec::new();
    for i in 0..turns {
        let at = match name {
            "a" => i % 5,
            _ if i % 7 == 6 => {
                pages.clear();
                continue;
            }
            _ => i % 3,
        };
        if pages.len() <= at {
            pages.resize(at + 1, None);
        }
        pages[at] = Some(i as u8);
    }
    pages
}

/// File `name`'s pages as `db` holds them.
async fn pages(db: &Session, name: &str) -> Result<Vec<Option<u8>>> {
    let file = db.file(name)?;
    let mut pages = Vec::new();
    for n in 0..file.len() / 4096 {
        pages.push(file.read(n).await?.map(|page| page[0]));
    }
    Ok(pages)
}

async fn rows(db: &mut Session) -> Result<usize> {
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
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
        let turns = rows(&mut db).await?;
        assert!(
            turns == acknowledged || turns == acknowledged + 1,
            "{case}: {turns} turns"
        );
        for name in ["a", "b"] {
            assert_eq!(
                pages(&db, name).await?,
                expected(name, turns),
                "{case}, {name}"
            );
        }
        // The namespace takes turns again.
        turn(&mut db, turns).await?;
        drop(db);
        let db = Session::open_with_options(store.clone(), options(None)).await?;
        assert_eq!(pages(&db, "a").await?, expected("a", turns + 1), "{case}");
        assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
    }
    Ok(Some(writes + 1))
}

/// The page keys of the first file a namespace made (id 0).
fn first_file_pages(store: &MemoryKv) -> usize {
    let keys = store.keys();
    keys.iter()
        .filter(|key| key.starts_with("v15/page/00000000/"))
        .count()
}

/// Every save checkpoints.
fn checkpointing(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        checkpoint_after: 0,
        ..StorageOptions::default()
    }
}

#[test]
fn an_emptied_file_loses_its_page_keys_at_the_next_checkpoint() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()));
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file("m")?;
        for n in 0..6 {
            file.write(n, Some(page(n as u8 + 1)))?;
        }
        assert_eq!(file.len(), 6 * 4096);
        db.save().await?;
        assert_eq!(first_file_pages(&store.inner), 6);

        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file("m")?;
        assert_eq!(file.read(5).await?, Some(page(6)));
        file.set_len(0)?;
        assert_eq!(file.read(5).await?, None);
        db.save().await?;
        assert_eq!(first_file_pages(&store.inner), 0);

        // Regrown, it reads zeros where it held pages.
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file("m")?;
        assert!(file.is_empty());
        file.write(7, Some(page(9)))?;
        db.save().await?;
        let db = Session::open(store.clone()).await?;
        let file = db.file("m")?;
        assert_eq!(file.len(), 8 * 4096);
        assert_eq!(file.read(5).await?, None);
        assert_eq!(file.read(7).await?, Some(page(9)));
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

#[test]
fn a_files_pages_are_cached_until_another_epoch() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let db =
            Session::open_with_options(store.clone(), checkpointing(Some(cache.clone()))).await?;
        let file = db.file("m")?;
        for n in 0..40 {
            file.write(n, Some(page(n as u8)))?;
        }
        db.save().await?;

        // The owner reads them back from the cache.
        store.reset_stats();
        let db =
            Session::open_with_options(store.clone(), checkpointing(Some(cache.clone()))).await?;
        let file = db.file("m")?;
        for n in 0..40 {
            assert_eq!(file.read(n).await?, Some(page(n as u8)));
        }
        assert_eq!(store.stats().kind("page").gets, 0);
        drop(db);

        // Another writer checkpoints: the owner's cached pages are of another
        // epoch, and it reads them from the store.
        let other = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        other.file("m")?.write(3, Some(page(99)))?;
        other.save().await?;
        store.reset_stats();
        let db =
            Session::open_with_options(store.clone(), checkpointing(Some(cache.clone()))).await?;
        let file = db.file("m")?;
        assert_eq!(file.read(3).await?, Some(page(99)));
        assert_eq!(file.read(4).await?, Some(page(4)));
        assert!(store.stats().kind("page").gets > 0);
        Ok(())
    })
}

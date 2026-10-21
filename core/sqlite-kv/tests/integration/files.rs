//! Paged files beside the SQL database (`Session::file`): written with the
//! tables, in one commit record, and kept like the database's own pages.
use bytes::Bytes;
use sqlite_kv::{
    Base, Durability, NamespaceCache, Result, Session, StorageOptions,
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
/// every seventh turn empties `b`, and page `i % 4` of `c` (over the golden
/// image) is its golden page with the first byte `i`.
async fn turn(db: &mut Session, i: usize) -> Result<()> {
    db.execute("BEGIN").await?;
    db.execute(&format!("INSERT INTO t VALUES({i})")).await?;
    db.file("a")?.write((i % 5) as u64, Some(page(i as u8)))?;
    let c = db.file_with_base("c", golden())?;
    c.write(
        (i % 4) as u64,
        Some(golden().changed((i % 4) as u64, i as u8)),
    )?;
    let b = db.file("b")?;
    match i % 7 {
        6 => b.set_len(0)?,
        _ => b.write((i % 3) as u64, Some(page(i as u8)))?,
    }
    db.seal(None).await.map(drop)
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
        pages[at] = Some(i as u8);
    }
    pages
}

/// File `name`'s pages as `db` holds them (each one's first byte).
async fn pages(db: &Session, name: &str) -> Result<Vec<Option<u8>>> {
    let file = match name {
        "c" => db.file_with_base(name, golden())?,
        _ => db.file(name)?,
    };
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
    let store = Arc::new(
        MonitorKv::new(FaultKv::new(MemoryKv::default(), nth as u64)).with_base("c", golden()),
    );
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
        let store =
            Arc::new(MonitorKv::seeded(memory.clone(), memory.values()).with_base("c", golden()));
        let case = format!("sync #{nth}, {kept} of {writes} writes kept, cached={cached}");
        let mut db = Session::open_with_options(store.clone(), options(None)).await?;
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

#[test]
fn a_file_over_a_base_reads_it_where_it_has_no_page() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()).with_base("m", golden()));
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file_with_base("m", golden())?;
        file.set_len(8 * 4096)?;
        assert_eq!(file.read(3).await?.as_deref(), golden().page(3));
        file.write(2, Some(golden().changed(2, 1)))?;
        file.write(5, Some(page(9)))?;
        // A page written back as its base's is a hole again.
        file.write(6, Some(golden().changed(6, 1)))?;
        file.write(6, Some(Bytes::copy_from_slice(golden().page(6).unwrap())))?;
        db.save().await?;

        let db = Session::open(store.clone()).await?;
        let file = db.file_with_base("m", golden())?;
        assert_eq!(file.read(2).await?, Some(golden().changed(2, 1)));
        assert_eq!(file.read(3).await?.as_deref(), golden().page(3));
        assert_eq!(file.read(5).await?, Some(page(9)));
        assert_eq!(file.read(6).await?.as_deref(), golden().page(6));
        // Two pages differ from the base: two page keys.
        assert_eq!(first_file_pages(&store.inner), 2);
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

#[test]
fn a_small_change_to_a_base_page_costs_little_in_the_log_and_in_place() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        // Logged, without checkpointing.
        let db = Session::open(store.clone()).await?;
        let file = db.file_with_base("m", golden())?;
        file.write(4, Some(golden().changed(4, 1)))?;
        store.reset_stats();
        db.save().await?;
        let logged = store.stats().bytes_written;
        assert!(logged < 300, "{logged} bytes logged");

        // In place, after a checkpoint.
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file_with_base("m", golden())?;
        file.write(4, Some(golden().changed(4, 2)))?;
        db.save().await?;
        let values = store.values();
        let (_, value) = (values.iter())
            .find(|(key, _)| key.starts_with("v15/page/00000000/"))
            .expect("the page in place");
        assert!(value.len() < 100, "{} bytes in place", value.len());
        let db = Session::open(store.clone()).await?;
        let file = db.file_with_base("m", golden())?;
        assert_eq!(file.read(4).await?, Some(golden().changed(4, 2)));
        Ok(())
    })
}

#[test]
fn a_file_opens_only_with_its_base_and_empties_without_it() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file_with_base("m", golden())?;
        file.write(1, Some(golden().changed(1, 1)))?;
        db.save().await?;

        let db = Session::open(store.clone()).await?;
        assert!(db.file_with_base("m", Golden::new(8, 8)).is_err());
        assert!(db.file("plain")?.read(0).await?.is_none());
        let file = db.file("m")?;
        assert!(file.read(1).await.is_err(), "it needs its base");
        assert!(file.write(1, Some(page(1))).is_err() || db.save().await.is_err());

        // Emptied without its base: no page keys are left.
        let db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        let file = db.file("m")?;
        file.write(1, None)?;
        file.set_len(0)?;
        db.save().await?;
        assert_eq!(first_file_pages(&store), 0);
        Ok(())
    })
}

#[test]
fn a_checkpoint_waits_for_a_based_file_to_be_opened() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MonitorKv::new(MemoryKv::default()).with_base("m", golden()));
        let db = Session::open(store.clone()).await?;
        db.file_with_base("m", golden())?
            .write(1, Some(golden().changed(1, 5)))?;
        db.save().await?;
        // Its log decodes only with its base: without it, no checkpoint.
        let mut db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        db.execute("CREATE TABLE t(n)").await?;
        db.save().await?;
        assert_eq!(store.inner.page_count(), 0, "no checkpoint");
        // Opened with it, the next save checkpoints the whole log.
        let mut db = Session::open_with_options(store.clone(), checkpointing(None)).await?;
        db.file_with_base("m", golden())?;
        db.execute("INSERT INTO t VALUES(1)").await?;
        db.save().await?;
        assert!(first_file_pages(&store.inner) == 1 && store.inner.page_count() > 1);
        let db = Session::open(store.clone()).await?;
        let file = db.file_with_base("m", golden())?;
        assert_eq!(file.read(1).await?, Some(golden().changed(1, 5)));
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

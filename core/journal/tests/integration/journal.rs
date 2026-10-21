//! A journal and its participants: the SQL as one of them, its records and
//! files beside it, made durable together by `Journal::commit`.
use bytes::Bytes;
use journal::{
    Journal, Result, Sql, StorageOptions,
    simulation::{MemoryKv, fresh_namespace},
};
use std::sync::Arc;

fn options() -> StorageOptions {
    StorageOptions::default()
}

async fn rows(sql: &mut Sql) -> Result<i64> {
    let rows = sql.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

/// Nothing a participant changes is durable until the journal commits, and
/// then all of it is, in one put.
#[test]
fn a_commit_makes_every_participant_durable_together() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&journal).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        sql.flush().await?;
        journal.record("state").set(Bytes::from_static(b"one"));
        journal
            .file("pages")?
            .write(0, Some(vec![1; 4096].into()))?;
        // Flushed, not committed: the store holds nothing.
        assert!(store.keys().is_empty(), "{:?}", store.keys());
        let puts = store.stats().puts;
        journal.commit().await?;
        assert_eq!(journal.version(), 1);
        assert_eq!(store.stats().puts - puts, 1, "one put");
        drop(sql);
        drop(journal);

        let journal = Journal::open_with_options(store.clone(), options()).await?;
        assert_eq!(journal.record("state").get(), &b"one"[..]);
        assert_eq!(journal.file("pages")?.read(0).await?.unwrap()[0], 1);
        let mut sql = Sql::open(&journal).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        // A turn whose commit never happens leaves nothing.
        sql.execute("INSERT INTO t VALUES(2)").await?;
        sql.flush().await?;
        journal.record("state").set(Bytes::from_static(b"two"));
        drop(sql);
        drop(journal);
        let journal = Journal::open_with_options(store, options()).await?;
        let mut sql = Sql::open(&journal).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        assert_eq!(sql.journal().record("state").get(), &b"one"[..]);
        Ok(())
    })
}

/// SQL commits stay in the engine until a flush moves them into the file: a
/// journal commit without one refuses, rather than leave them out.
#[test]
fn a_commit_refuses_sql_not_flushed() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&journal).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        let refused = journal.commit().await.unwrap_err().to_string();
        assert!(refused.contains("Sql::flush"), "{refused}");
        assert!(store.keys().is_empty());
        Ok(())
    })
}

/// A journal is usable without SQL, and one SQL runs on it at most.
#[test]
fn a_journal_needs_no_sql_and_takes_one() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        journal.record("only").set(Bytes::from_static(b"records"));
        journal.commit().await?;
        let sql = Sql::open(&journal).await?;
        assert!(Sql::open(&journal).await.is_err());
        drop(sql);
        let journal = Journal::open(store).await?;
        assert_eq!(journal.record("only").get(), &b"records"[..]);
        assert_eq!(journal.version(), 1);
        Ok(())
    })
}

/// With an owner cache, the engine a session leaves idle waits for the next
/// journal's SQL, past a journal that opened no SQL.
#[test]
fn the_idle_engine_waits_for_the_next_sql() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StorageOptions {
            cache: Some(cache.clone()),
            ..options()
        };
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&journal).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.flush().await?;
        journal.commit().await?;
        drop(sql);
        drop(journal);
        // A journal without SQL leaves the engine where it was.
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        assert_eq!(journal.version(), 1);
        drop(journal);
        let journal = Journal::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&journal).await?;
        assert!(sql.reused_engine());
        sql.execute("INSERT INTO t VALUES(1)").await?;
        sql.flush().await?;
        journal.commit().await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

//! A volume and its participants: the SQL as one of them, its records and
//! files beside it, made durable together by `Volume::commit`.
use bytes::Bytes;
use sql::Sql;
use std::sync::Arc;
use volume::{
    Result, StorageOptions, Volume,
    simulation::{MemoryKv, fresh_namespace},
};

fn options() -> StorageOptions {
    StorageOptions::default()
}

async fn rows(sql: &mut Sql) -> Result<i64> {
    let rows = sql.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

/// Nothing a participant changes is durable until the volume commits, and
/// then all of it is, in one put.
#[test]
fn a_commit_makes_every_participant_durable_together() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        sql.flush().await?;
        volume.record("state").set(Bytes::from_static(b"one"));
        volume.file("pages")?.write(0, Some(vec![1; 4096].into()))?;
        // Flushed, not committed: the store holds nothing.
        assert!(store.keys().is_empty(), "{:?}", store.keys());
        let puts = store.stats().puts;
        volume.commit().await?;
        assert_eq!(volume.version(), 1);
        assert_eq!(store.stats().puts - puts, 1, "one put");
        drop(sql);
        drop(volume);

        let volume = Volume::open_with_options(store.clone(), options()).await?;
        assert_eq!(volume.record("state").get(), &b"one"[..]);
        assert_eq!(volume.file("pages")?.read(0).await?.unwrap()[0], 1);
        let mut sql = Sql::open(&volume).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        // A turn whose commit never happens leaves nothing.
        sql.execute("INSERT INTO t VALUES(2)").await?;
        sql.flush().await?;
        volume.record("state").set(Bytes::from_static(b"two"));
        drop(sql);
        drop(volume);
        let volume = Volume::open_with_options(store, options()).await?;
        let mut sql = Sql::open(&volume).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        assert_eq!(sql.volume().record("state").get(), &b"one"[..]);
        Ok(())
    })
}

/// SQL commits stay in the engine until a flush moves them into the file: a
/// volume commit without one refuses, rather than leave them out.
#[test]
fn a_commit_refuses_sql_not_flushed() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        let refused = volume.commit().await.unwrap_err().to_string();
        assert!(
            refused.contains("\"sql\" has changes not yet in the volume"),
            "{refused}"
        );
        assert!(store.keys().is_empty());
        Ok(())
    })
}

/// A volume is usable without SQL, and one SQL runs on it at most.
#[test]
fn a_volume_needs_no_sql_and_takes_one() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        volume.record("only").set(Bytes::from_static(b"records"));
        volume.commit().await?;
        let sql = Sql::open(&volume).await?;
        assert!(Sql::open(&volume).await.is_err());
        drop(sql);
        let volume = Volume::open(store).await?;
        assert_eq!(volume.record("only").get(), &b"records"[..]);
        assert_eq!(volume.version(), 1);
        Ok(())
    })
}

/// With an owner cache, the engine a session leaves idle waits for the next
/// volume's SQL, past a volume that opened no SQL.
#[test]
fn the_idle_engine_waits_for_the_next_sql() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StorageOptions {
            cache: Some(cache.clone()),
            ..options()
        };
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.flush().await?;
        volume.commit().await?;
        drop(sql);
        drop(volume);
        // A volume without SQL leaves the engine where it was.
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        assert_eq!(volume.version(), 1);
        drop(volume);
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        assert!(sql.reused_engine());
        sql.execute("INSERT INTO t VALUES(1)").await?;
        sql.flush().await?;
        volume.commit().await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// An open SQL transaction holds changes too, though the engine keeps them to
/// itself until `COMMIT`: the volume refuses to commit around it.
#[test]
fn a_commit_refuses_an_open_sql_transaction() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.flush().await?;
        volume.commit().await?;
        sql.execute("BEGIN").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let refused = volume.commit().await.unwrap_err().to_string();
        assert!(refused.contains("not yet in the volume"), "{refused}");
        sql.flush().await?;
        volume.commit().await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// A `COMMIT` that fails with an SQL error leaves its transaction open and
/// takes nothing from the volume: what an earlier flush moved still commits.
#[test]
fn a_failed_commit_at_flush_keeps_what_was_flushed() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let volume = Volume::open_with_options(store.clone(), options()).await?;
        let mut sql = Sql::open(&volume).await?;
        sql.execute("PRAGMA foreign_keys = ON").await?;
        sql.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
            .await?;
        sql.execute(
            "CREATE TABLE t(n INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
        )
        .await?;
        sql.execute("INSERT INTO parent VALUES(1)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        sql.flush().await?;
        sql.execute("BEGIN").await?;
        sql.execute("INSERT INTO t VALUES(2)").await?;
        let error = sql.flush().await.unwrap_err().to_string();
        assert!(error.to_lowercase().contains("foreign key"), "{error}");
        assert!(sql.is_usable());
        assert!(sql.in_transaction(), "the transaction stays open");
        sql.execute("ROLLBACK").await?;
        sql.flush().await?;
        volume.commit().await?;
        drop(sql);
        drop(volume);
        let volume = Volume::open_with_options(store, options()).await?;
        let mut sql = Sql::open(&volume).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

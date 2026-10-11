use sqlite_kv::{Result, Session, simulation::MemoryKv};
use std::sync::Arc;

#[test]
fn save_checkpoints_private_wal_and_reopens_database_only() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open(store.clone()).await?;
        db.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        db.execute("INSERT INTO items VALUES (1, 'one'), (2, 'two')")
            .await?;
        assert!(db.io_stats().private_wal_len > 0);
        assert!(db.io_stats().private_wal_bytes_written > 0);
        assert_eq!(store.stats().puts, 0, "local WAL must not reach backend");
        db.save().await?;
        assert_eq!(store.stats().puts, 1, "a save is one log append");
        store.crash(&[]); // every acknowledged put survives

        let mut reopened = Session::open(store.clone()).await?;
        assert_eq!(reopened.io_stats().private_wal_len, 0);
        let rows = reopened
            .query("SELECT value FROM items ORDER BY id")
            .await?;
        assert_eq!(
            rows.iter().map(|r| r[0].to_string()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(
            reopened.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        Ok(())
    })
}

#[test]
fn dropping_overlay_preserves_saved_state_and_sql_rollback_works() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open(store.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        db.execute("INSERT INTO t VALUES(1, 10)").await?;
        db.checkpoint().await?;
        db.save().await?;

        let mut discarded = Session::open(store.clone()).await?;
        discarded.execute("UPDATE t SET n=99").await?;
        drop(discarded); // committed locally, never published
        let mut db = Session::open(store.clone()).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        db.execute("BEGIN").await?;
        db.execute("UPDATE t SET n=50").await?;
        db.execute("ROLLBACK").await?;
        db.checkpoint().await?;
        db.save().await?;
        let mut db = Session::open(store).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
        Ok(())
    })
}

#[test]
fn save_rejects_open_transaction() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open(store).await?;
        db.execute("BEGIN").await?;
        assert!(
            db.save()
                .await
                .unwrap_err()
                .to_string()
                .contains("open SQL transaction")
        );
        Ok(())
    })
}

#[test]
fn cold_indexed_lookup_is_lazy() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open(store.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT, payload TEXT)")
            .await?;
        db.execute("CREATE INDEX t_label ON t(label)").await?;
        // Incompressible payloads make this a large save, which checkpoints itself.
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4000) INSERT INTO t SELECT x, printf('label-%06d',x), randomblob(400) FROM n").await?;
        db.checkpoint().await?;
        db.save().await?;
        let total = store.stored_bytes();
        store.reset_stats();
        let mut db = Session::open(store.clone()).await?;
        let sql = "SELECT id FROM t WHERE label='label-002341'";
        let rows = db.query(sql).await?;
        assert_eq!(rows[0][0].to_string(), "2341");
        let stats = store.stats();
        assert!(
            store.page_count() * 4096 > 1_000_000,
            "a large save checkpoints itself"
        );
        assert!(stats.by_kind["page"].gets < store.page_count() as u64 / 10);
        assert!(stats.bytes_read < total as u64 / 10, "{stats:?}");
        assert!(stats.gets > 0);
        store.reset_stats();
        db.query(sql).await?;
        assert_eq!(
            store.stats().gets,
            0,
            "warm page cache should avoid backend reads"
        );
        Ok(())
    })
}

#[test]
fn repeated_local_commits_publish_only_final_database_blocks() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut db = Session::open(store.clone()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER)")
            .await?;
        db.execute("INSERT INTO t VALUES(1,0)").await?;
        for n in 1..=100 {
            db.execute(&format!("UPDATE t SET n={n}")).await?;
        }
        db.execute("BEGIN").await?;
        db.execute("SAVEPOINT s").await?;
        db.execute("UPDATE t SET n=999").await?;
        db.execute("ROLLBACK TO s").await?;
        db.execute("RELEASE s").await?;
        db.execute("COMMIT").await?;
        let wal = db.io_stats().private_wal_bytes_written;
        assert!(wal > 100 * 4096);
        assert_eq!(store.stats().puts, 0);
        db.checkpoint().await?;
        assert_eq!(db.io_stats().private_wal_len, 0);
        assert_eq!(store.stats().puts, 0, "checkpoint is still private");
        let database_len = db.io_stats().database_len;
        db.save().await?;
        let stats = store.stats();
        // One log segment carrying each final page once.
        assert_eq!(stats.puts, 1);
        assert!(database_len.div_ceil(4096) >= 2);
        assert!(stats.bytes_written < wal / 10);
        store.crash(&[]);
        let mut db = Session::open(store).await?;
        assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "100");
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        println!(
            "100 updates: {wal} local WAL bytes, {} backend bytes in one segment",
            stats.bytes_written
        );
        Ok(())
    })
}

/// Pages larger than a 4096-byte block survive a save and a cold reopen.
#[test]
fn large_page_sizes_reopen_after_a_save() -> Result<()> {
    futures::executor::block_on(async {
        for page_size in [8192, 65536] {
            let store: Arc<dyn sqlite_kv::kv::KvStore> =
                Arc::new(sqlite_kv::simulation::MemoryKv::default());
            let mut db = Session::open(store.clone()).await?;
            db.execute(&format!("PRAGMA page_size = {page_size}"))
                .await?;
            db.execute("CREATE TABLE t(v TEXT)").await?;
            db.execute("INSERT INTO t VALUES ('kept')").await?;
            db.save().await?;
            let mut db = Session::open(store.clone()).await?;
            let rows = db.query("SELECT v FROM t").await?;
            assert_eq!(rows[0][0].to_string(), "kept", "page size {page_size}");
            let rows = db.query("PRAGMA page_size").await?;
            assert_eq!(rows[0][0].as_int(), Some(page_size));
        }
        Ok(())
    })
}

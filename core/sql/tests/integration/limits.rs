//! Memory limits: private changes and result rows.
use crate::host::Db;
use sql::SqlOptions;
use state::{Result, StorageOptions, simulation::MemoryKv};
use std::sync::Arc;

#[test]
fn private_changes_are_bounded() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = StorageOptions::default;
        let limits = || SqlOptions {
            max_private_bytes: 1 << 20,
            ..SqlOptions::default()
        };
        let mut db = Db::open_with(store.clone(), options(), limits()).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        db.execute("INSERT INTO t VALUES(1, randomblob(100000))")
            .await?;
        db.save().await?;
        let mut db = Db::open_with(store.clone(), options(), limits()).await?;
        let error = db
            .execute("INSERT INTO t SELECT value, randomblob(100000) FROM (WITH RECURSIVE n(value) AS (VALUES(2) UNION ALL SELECT value + 1 FROM n WHERE value < 40) SELECT value FROM n)")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("max_private_bytes"), "{error}");
        assert!(db.execute("SELECT 1").await.is_err(), "poisoned");
        drop(db);
        let mut db = Db::open_with(store, options(), limits()).await?;
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(1)
        );
        Ok(())
    })
}

#[test]
fn result_rows_are_bounded() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let limits = SqlOptions {
            max_result_bytes: 64 << 10,
            ..SqlOptions::default()
        };
        let mut db = Db::open_with(store, StorageOptions::default(), limits).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        for i in 0..20 {
            db.execute(&format!("INSERT INTO t VALUES({i}, zeroblob(10000))"))
                .await?;
        }
        assert_eq!(db.query("SELECT id FROM t").await?.len(), 20);
        let error = db.query("SELECT body FROM t").await.unwrap_err();
        assert!(error.to_string().contains("max_result_bytes"), "{error}");
        Ok(())
    })
}

/// The limit is on private changes, not on moving committed ones into a
/// WAL: a flush, which copies the engine WAL into the database's pages, passes.
#[test]
fn a_transaction_within_the_limit_flushes() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let limits = SqlOptions {
            max_private_bytes: 1 << 20,
            ..SqlOptions::default()
        };
        let mut db = Db::open_with(store.clone(), StorageOptions::default(), limits).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, body BLOB)")
            .await?;
        // About 600 KiB: over half the limit, which the WAL and the
        // checkpoint's pages together would exceed.
        db.execute("INSERT INTO t SELECT value, randomblob(100000) FROM (WITH RECURSIVE n(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM n WHERE value < 6) SELECT value FROM n)")
            .await?;
        db.save().await?;
        let mut db = Db::open_with(store, StorageOptions::default(), SqlOptions::default()).await?;
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(6)
        );
        Ok(())
    })
}

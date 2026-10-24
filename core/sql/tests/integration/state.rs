//! A state and SQL on it: SQL's file, records and other files beside it,
//! made durable together by a mutation's flush.
use crate::host::StateSetup;
use bytes::Bytes;
use sql::Sql;
use state::{
    Result, State, StateOptions,
    simulation::{MemoryKv, fresh_namespace},
};
use std::sync::Arc;

fn options() -> StateOptions {
    StateOptions::default()
}

async fn rows(sql: &mut Sql) -> Result<i64> {
    let rows = sql.query("SELECT count(*) FROM t").await?;
    Ok(rows[0][0].as_int().unwrap())
}

/// Nothing SQL or a mutation changes is durable until the mutation flushes, and then
/// all of it is, under one root; the state reads as last flushed meanwhile.
#[test]
fn a_flush_makes_sql_and_the_rest_durable_together() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.set(&state.record("progress"), Bytes::from_static(b"one"))?;
        mutation.write(&state.file("pages")?, 0, Some(vec![1; 4096].into()))?;
        // In the mutation, not flushed: the store holds nothing, and the state
        // reads as before.
        assert!(store.keys().is_empty(), "{:?}", store.keys());
        assert!(state.record("progress").get().is_empty());
        assert!(state.file("pages")?.is_empty());
        let puts = store.stats().puts;
        mutation.flush().await?;
        assert_eq!(state.version(), 1);
        // A blob for each file's chunk (the record is inline), then the root.
        assert_eq!(store.stats().puts - puts, 3);
        assert_eq!(store.stats().kind("root").puts, 1);
        assert_eq!(state.record("progress").get(), &b"one"[..]);
        drop(sql);
        drop(state);

        let state = StateSetup::from(options()).open(store.clone()).await?;
        assert_eq!(state.record("progress").get(), &b"one"[..]);
        assert_eq!(state.file("pages")?.read(0).await?.unwrap()[0], 1);
        let mut sql = Sql::open(&state).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        // A turn whose mutation never flushes leaves nothing.
        sql.execute("INSERT INTO t VALUES(2)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.set(&state.record("progress"), Bytes::from_static(b"two"))?;
        drop(mutation);
        drop(sql);
        drop(state);
        let state = StateSetup::from(options()).open(store).await?;
        let mut sql = Sql::open(&state).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        assert_eq!(sql.state().record("progress").get(), &b"one"[..]);
        Ok(())
    })
}

/// SQL commits stay in the engine until [`Sql::flush`] moves them into a
/// mutation: one flushed without it leaves them private, for a later one.
#[test]
fn a_flush_without_sql_leaves_its_changes_private() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("progress"), Bytes::from_static(b"one"))?;
        mutation.flush().await?;
        assert!(
            state.file(sql::DB_FILE)?.is_empty(),
            "SQL's changes are private"
        );
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        drop(sql);
        drop(state);
        let state = StateSetup::from(options()).open(store).await?;
        let mut sql = Sql::open(&state).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        assert_eq!(state.version(), 2);
        Ok(())
    })
}

/// A state is usable without SQL, and one SQL runs on it at most.
#[test]
fn a_state_needs_no_sql_and_takes_one() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("only"), Bytes::from_static(b"records"))?;
        mutation.flush().await?;
        let sql = Sql::open(&state).await?;
        let error = Sql::open(&state).await.err().unwrap().to_string();
        assert!(error.contains("already runs on this state"), "{error}");
        drop(sql);
        drop(Sql::open(&state).await?);
        let state = State::open(store).await?;
        assert_eq!(state.record("only").get(), &b"records"[..]);
        assert_eq!(state.version(), 1);
        Ok(())
    })
}

/// With an owner cache, the engine a session parks waits for the next
/// state's SQL, past a state that opened no SQL.
#[test]
fn a_parked_engine_waits_for_the_next_sql() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StateSetup {
            options: options(),
            cache: Some(cache.clone()),
        };
        let state = options().open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        drop(sql);
        drop(state);
        // A state without SQL leaves the engine where it was.
        let state = options().open(store.clone()).await?;
        assert_eq!(state.version(), 1);
        drop(state);
        let state = options().open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        assert!(sql.reused_engine());
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// An open SQL transaction stays open across a flush without SQL; the
/// next [`Sql::flush`] commits it.
#[test]
fn an_open_sql_transaction_outlives_a_flush_without_sql() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        sql.execute("BEGIN").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        mutation.set(&state.record("progress"), Bytes::from_static(b"one"))?;
        mutation.flush().await?;
        assert!(sql.in_transaction());
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        assert!(!sql.in_transaction());
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// A `COMMIT` that fails with an SQL error leaves its transaction open and
/// takes nothing from the mutation: what an earlier flush moved still flushes.
#[test]
fn a_failed_commit_at_flush_keeps_what_was_flushed() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("PRAGMA foreign_keys = ON").await?;
        sql.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
            .await?;
        sql.execute(
            "CREATE TABLE t(n INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
        )
        .await?;
        sql.execute("INSERT INTO parent VALUES(1)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        sql.execute("BEGIN").await?;
        sql.execute("INSERT INTO t VALUES(2)").await?;
        let error = sql.flush(&mut mutation).await.unwrap_err().to_string();
        assert!(error.to_lowercase().contains("foreign key"), "{error}");
        assert!(sql.is_usable());
        assert!(sql.in_transaction(), "the transaction stays open");
        sql.execute("ROLLBACK").await?;
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        drop(sql);
        drop(state);
        let state = StateSetup::from(options()).open(store).await?;
        let mut sql = Sql::open(&state).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// Between `Sql::flush` into a mutation and the mutation's flush, a query reads the
/// moved rows, from the session's private view: the state's file `db` holds
/// them only once the mutation flushes, and the session reads them there then.
#[test]
fn a_query_reads_the_rows_flushed_into_a_mutation_before_the_mutation_flushes() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER)").await?;
        sql.execute("INSERT INTO t VALUES(1)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        assert_eq!(rows(&mut sql).await?, 1, "in the mutation");
        assert!(state.file("db")?.is_empty(), "not flushed");
        assert!(store.keys().is_empty());
        mutation.flush().await?;
        assert!(!state.file("db")?.is_empty());
        assert_eq!(rows(&mut sql).await?, 1, "flushed");
        Ok(())
    })
}

/// Once the state flushed the mutation, the session is clean: it leaves its engine
/// to the next state's session. With the mutation not flushed by the state, the
/// moved pages are the session's own, and it leaves no engine.
#[test]
fn a_session_is_clean_once_the_state_flushed_its_mutation() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let cache = fresh_namespace();
        let options = || StateSetup {
            options: options(),
            cache: Some(cache.clone()),
        };
        let state = options().open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(1)")
            .await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        drop((sql, state));
        let state = options().open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        assert!(sql.reused_engine(), "clean after the flush");
        assert_eq!(rows(&mut sql).await?, 1);
        sql.execute("INSERT INTO t VALUES(2)").await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        drop((sql, mutation, state));
        let state = options().open(store).await?;
        let mut sql = Sql::open(&state).await?;
        assert!(
            !sql.reused_engine(),
            "the state had not flushed the mutation"
        );
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

/// A mutation dropped after `Sql::flush` loses nothing of the SQL: the moved pages
/// are private changes again, which the next flush carries.
#[test]
fn a_dropped_mutation_leaves_the_sql_pages_to_the_next_flush() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = StateSetup::from(options()).open(store.clone()).await?;
        let mut sql = Sql::open(&state).await?;
        sql.execute("CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(1)")
            .await?;
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        drop(mutation);
        assert_eq!(rows(&mut sql).await?, 1);
        assert!(state.file("db")?.is_empty() && state.version() == 0);
        let mut mutation = state.mutation();
        sql.flush(&mut mutation).await?;
        mutation.flush().await?;
        assert_eq!(state.version(), 1);
        drop(sql);
        drop(state);
        let state = StateSetup::from(options()).open(store).await?;
        let mut sql = Sql::open(&state).await?;
        assert_eq!(rows(&mut sql).await?, 1);
        Ok(())
    })
}

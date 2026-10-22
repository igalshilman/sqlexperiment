//! Cancellation at every point: a call dropped after any number of polls
//! ends its session, and the store holds the state before it or after it.
use crate::{
    fault::poll_at_most,
    host::{Db, Durability, Setup},
    reference::{dump, integrity},
};
use bytes::Bytes;
use sql::{Sql, binding::Session};
use state::{
    NamespaceCache, Result, State, StorageOptions,
    kv::KvStore,
    simulation::{MemoryKv, fresh_namespace},
};
use std::sync::Arc;

/// Pages read from the store as queries need them; the log checkpointed often.
fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        checkpoint_after: 2,
        prefetch_pages: 0,
        hint_pages: 0,
        ..StorageOptions::default()
    }
}

/// A store with a table of 20 rows, some past a page, its pages checkpointed.
async fn baseline() -> Result<MemoryKv> {
    let memory = MemoryKv::default();
    let mut db = Db::open(
        Arc::new(memory.clone()),
        Setup(options(None), Durability::Commit),
    )
    .await?;
    db.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body TEXT); CREATE INDEX t_n ON t(n)",
    )
    .await?;
    db.execute("WITH RECURSIVE x(i) AS (VALUES(0) UNION ALL SELECT i + 1 FROM x WHERE i < 19) INSERT INTO t SELECT i, i, substr(printf('%07000d', i), 1, 100 + (i % 5) * 1500) FROM x")
        .await?;
    let effects = db.state().record("effects");
    db.wal().set(&effects, Bytes::from("before"))?;
    for k in 0..3 {
        db.execute(&format!("CREATE TABLE a{k}(x)")).await?;
    }
    // Flushes that leave the first page alone, so that the log no longer
    // holds it: it is read from the store.
    for k in 0..6 {
        db.execute(&format!("UPDATE t SET n = {k} WHERE id = 1"))
            .await?;
    }
    Ok(memory)
}

/// The state of a store and its effects, through a plain session; its
/// database checks out.
async fn observe(store: MemoryKv) -> Result<(String, Bytes)> {
    let state = State::open_with_options(Arc::new(store), options(None)).await?;
    let mut sql = Sql::open(&state).await?;
    assert_eq!(integrity(&mut sql).await?, "ok");
    Ok((dump(&mut sql).await?, state.record("effects").get()))
}

/// A call to cancel, on a session it opens and prepares.
#[derive(Clone, Copy, Debug)]
enum Call {
    /// A statement through [`Sql`], flushed after it (`Durability::Commit`).
    Statement,
    /// A turn's flush: [`Sql::flush`] into a WAL, a record set in it, and
    /// [`state::Wal::flush`].
    Turn,
    /// [`Session::query`] of a write, which commits before it returns.
    SessionWrite,
    /// [`Session::query`] of a read.
    SessionRead,
    /// [`Session::commit`] of a transaction, with effects.
    SessionCommit,
}

const UPDATE: &str = "UPDATE t SET n = n + 1000, body = printf('%07000d', id) WHERE id % 7 = 0";
/// A change that leaves the database's first page alone.
const IN_PLACE: &str = "INSERT INTO a0 VALUES(1)";

/// A session of either kind.
enum Open {
    Sql(Db),
    Session(Session),
}

impl Open {
    fn is_usable(&self) -> bool {
        match self {
            Open::Sql(db) => db.is_usable(),
            Open::Session(session) => session.is_usable(),
        }
    }

    async fn query(&mut self, sql: &str) -> Result<()> {
        match self {
            Open::Sql(db) => db.execute(sql).await,
            Open::Session(session) => session.query(sql, None).await.map(drop),
        }
    }
}

impl Call {
    async fn open(self, store: MemoryKv, cache: Option<NamespaceCache>) -> Result<Open> {
        let store: Arc<dyn KvStore> = Arc::new(store);
        let mut open = match self {
            Call::Statement | Call::Turn => {
                let durability = match self {
                    Call::Turn => Durability::Save,
                    _ => Durability::Commit,
                };
                let setup = Setup(options(cache), durability);
                Open::Sql(Db::open(store, setup).await?)
            }
            _ => Open::Session(Session::open_with_options(store, options(cache), false).await?),
        };
        match (self, &mut open) {
            (Call::Turn, Open::Sql(db)) => {
                // A change that leaves the first page alone, whose move
                // reads it again, from the store.
                db.execute(IN_PLACE).await?;
                db.execute("SELECT sum(length(body)) FROM t").await?;
            }
            (Call::SessionCommit, Open::Session(session)) => {
                session.begin().await?;
                session.query(UPDATE, None).await?;
            }
            _ => {}
        }
        Ok(open)
    }

    async fn run(self, open: &mut Open) -> Result<()> {
        match (self, open) {
            (Call::Statement, Open::Sql(db)) => db.execute(UPDATE).await,
            (Call::Turn, Open::Sql(db)) => {
                let effects = db.state().record("effects");
                db.wal().set(&effects, Bytes::from("after"))?;
                db.flush().await.map(drop)
            }
            (Call::SessionWrite, Open::Session(session)) => {
                session.query(UPDATE, None).await.map(drop)
            }
            (Call::SessionRead, Open::Session(session)) => {
                let sql = "SELECT sum(length(body)), sum(n) FROM t";
                session.query(sql, None).await.map(drop)
            }
            (Call::SessionCommit, Open::Session(session)) => {
                session.commit(Some(Bytes::from("after"))).await
            }
            _ => unreachable!(),
        }
    }
}

/// Drop each call after one poll, two, ... until it finishes: the session
/// ends (later calls fail), and the store holds the state before the call or,
/// if it wrote, the one after it, through a crash keeping any prefix of the
/// writes, and through the owner's cache.
#[test]
fn a_call_cancelled_after_any_number_of_polls_ends_its_session() -> Result<()> {
    futures::executor::block_on(async {
        let base = baseline().await?;
        let before = observe(base.fork()).await?;
        let mut cases = 0;
        for call in [
            Call::Statement,
            Call::Turn,
            Call::SessionWrite,
            Call::SessionRead,
            Call::SessionCommit,
        ] {
            for cached in [false, true] {
                let after = {
                    let memory = base.fork();
                    let mut open = call.open(memory.clone(), None).await?;
                    call.run(&mut open).await?;
                    drop(open);
                    observe(memory).await?
                };
                for polls in 1.. {
                    let case = format!("{call:?} dropped after {polls} polls, cached={cached}");
                    let memory = base.fork();
                    let cache = cached.then(fresh_namespace);
                    let mut open = call.open(memory.clone(), cache.clone()).await?;
                    let writes = memory.stats().puts + memory.stats().deletes;
                    if let Some(result) = poll_at_most(call.run(&mut open), polls) {
                        result?;
                        assert!(open.is_usable(), "{case}");
                        assert!(polls > 3, "{case}: too few points to cancel at");
                        break;
                    }
                    let wrote = memory.stats().puts + memory.stats().deletes > writes;
                    assert!(!open.is_usable(), "{case}: still usable");
                    assert!(open.query("SELECT 1").await.is_err(), "{case}: ran SQL");
                    drop(open);
                    // What reads see, and what each crash keeps.
                    let unsynced = memory.unsynced().len();
                    let current = observe(memory.fork()).await?;
                    let mut seen = vec![current.clone()];
                    for kept in 0..unsynced {
                        let crashed = memory.fork();
                        crashed.crash_keeping(kept);
                        seen.push(observe(crashed).await?);
                    }
                    for state in seen {
                        assert!(
                            state == before || (wrote && state == after),
                            "{case}: neither before nor after:\n{}",
                            state.0
                        );
                    }
                    // The process goes on: the next session of the owner,
                    // through its cache, sees what the store holds.
                    if cached {
                        let state =
                            State::open_with_options(Arc::new(memory), options(cache)).await?;
                        let mut sql = Sql::open(&state).await?;
                        assert_eq!(
                            dump(&mut sql).await?,
                            current.0,
                            "{case}: through the cache"
                        );
                    }
                    cases += 1;
                }
            }
        }
        println!("cancellation sweep: {cases} cases");
        Ok(())
    })
}

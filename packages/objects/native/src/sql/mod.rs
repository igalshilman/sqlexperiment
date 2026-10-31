//! An object's database: SQLite-compatible SQL, Turso's Rust engine
//! (`turso_core`, unmodified), over the [`State`]'s file, committed with
//! the effects in the state's flush.
//!
//! For a Restate object without Restate's journal (`journalReplay: false`),
//! whose state is the namespace. A [`Database`] is one session of it: its
//! statements change a private view of the file, and each call that leaves
//! no SQL transaction open commits: it moves what the SQL committed into a
//! [`Mutation`], with the effects of [`Database::commit`] if any, and flushes
//! it, durable before the call returns.
//!
//! | Layer | Representation | Survives? |
//! |---|---|---|
//! | SQL working state | Dirty 4096-byte pages and the private engine WAL | No |
//! | A commit | The pages and effects in one mutation, flushing | No: one that fails ends the session |
//! | The state | Its head, tail and packs (see [`state`](crate::state)) | Yes, after an acknowledged flush |
//!
//! An SQL error (syntax, a constraint, a missing table) undoes just that
//! statement, as in SQLite, and the session goes on. A storage error, a
//! cancelled call or a failed flush ends the session ([`Database::is_usable`]
//! returns false): settle outstanding requests and open a new one.
//!
//! # Engine reuse
//!
//! With a [`PageCache`], a session that ends cleanly leaves its idle engine
//! there ([`State::park`]); the next session of the namespace reuses it when
//! nothing changed, keeping the parsed schema and the engine's page cache.
//!
//! # Driving the engine
//!
//! The caller's task drives nonblocking engine open/connect state machines and
//! statement steps, awaits queued state reads, then resumes the engine.
//! Completions run on that task. No worker thread, command channel or
//! background task is created. Cached queries yield to the executor after 64
//! steps; an individual engine call is not preemptible. Calls on a session
//! run one at a time.
//!
//! The pinned engine also has synchronous metadata helpers. Open asynchronously
//! retains page zero for these reads; dirty overlay bytes always take priority.
//! `IO::step` serves only resident reads and local sync barriers. A synchronous
//! nonresident read or file mutation fails explicitly, without starting I/O.
//! Such an engine path cannot be suspended by this adapter; the session must be
//! discarded. Async query/checkpoint paths continue through the host I/O pump.
//!
//! The engine holds an owned schema lock across open-time I/O. The `parking_lot`
//! `send_guard` feature keeps the open future `Send` when the executor moves it
//! between threads. No unsafe future wrapper or engine-source patch is used.
//!
//! # Example
//!
//! ```ignore
//! # use crate::sql::Database;
//! # async fn example(db: Database) -> crate::state::Result<()> {
//! db.execute("CREATE TABLE messages(body TEXT)").await?; // durable now
//! db.begin().await?;
//! db.execute("INSERT INTO messages VALUES('hello')").await?;
//! db.commit(Some("sends".into())).await?; // the row and the effects, durable together
//! db.save().await?; // ends the session, leaving its engine to the next one
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! A session's private changes and a statement's result rows are capped by
//! [`SqlOptions::max_private_bytes`] and [`SqlOptions::max_result_bytes`]:
//! going past either ends the session.
//!
//! # Features
//!
//! In tests, and with the crate's `bench` feature: the state's in-memory
//! store, fault injection and latency simulator, and the I/O stats here.
pub mod io;

/// SQL values, as returned in rows and bound as parameters.
pub use turso_core::{Numeric, Value};

use crate::sql::io::{DB_PATH, KvIo, MountedIo};
use crate::state::{File, Mutation, PageCache, PageCacheConfig, State, kv::KvStore};
use bytes::Bytes;
use futures::lock::MutexGuard;
use std::{
    collections::HashSet,
    future::poll_fn,
    path::PathBuf,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};
use turso_core::storage::database::DatabaseFile;
use turso_core::{
    ConnectAsyncState, Connection, IO, IOResult, OpenFlags, OpenOptions, SqliteDialect, StepResult,
    TempStore,
};

/// Default [`SqlOptions::max_private_bytes`].
pub const MAX_PRIVATE_BYTES: usize = 256 << 20;
/// Default [`SqlOptions::max_result_bytes`].
pub const MAX_RESULT_BYTES: usize = 64 << 20;

/// Per-session SQL limits.
#[derive(Clone, Debug)]
pub struct SqlOptions {
    /// Most bytes of changes not yet flushed and private engine WAL a session
    /// may hold; a write beyond it ends the session.
    pub max_private_bytes: usize,
    /// Most bytes of result rows one statement may collect; beyond it the
    /// session ends.
    pub max_result_bytes: usize,
}

impl Default for SqlOptions {
    fn default() -> Self {
        Self {
            max_private_bytes: MAX_PRIVATE_BYTES,
            max_result_bytes: MAX_RESULT_BYTES,
        }
    }
}

/// Local engine traffic, distinct from durable backend traffic.
#[cfg(any(test, feature = "bench"))]
#[derive(Clone, Copy, Debug)]
pub struct IoStats {
    pub database_bytes_written: u64,
    pub private_wal_bytes_written: u64,
    pub database_len: u64,
    pub private_wal_len: u64,
}

/// Adapter, engine and state errors returned to the host task.
pub type Result<T> = crate::state::Result<T>;
/// Result rows, each a value per column.
pub type Rows = Vec<Vec<Value>>;

/// The SQL engine over a state's file: one private SQL view, driven
/// entirely by the caller's async task, which a [`Database`] holds.
///
/// The host advances engine state machines, awaits queued KV I/O and resumes the
/// engine on the same task. No worker thread or background task is created.
/// An SQL error leaves the engine usable; a storage error, a cancelled call
/// or a failed flush ends it (see [`Self::is_usable`]). Its changes become
/// durable only with a mutation's [`Mutation::flush`], after [`Self::flush`]
/// into it; dropping it leaves the engine to the next session.
struct Engine {
    state: State,
    database: Option<Arc<turso_core::Database>>,
    connection: Option<Arc<Connection>>,
    io: Arc<KvIo>,
    /// See [`SqlOptions::max_result_bytes`].
    max_result_bytes: usize,
    poisoned: bool,
    /// Inside a `transaction()` body, from `begin` to its `commit` or
    /// `rollback`: a statement that ends the transaction fails, and ends the
    /// engine, rather than commit the body halfway.
    in_body: bool,
    /// The engine moved into the owner cache; nothing is left to shut down.
    parked: bool,
    /// No SQL changed per-connection state (settings, attachments, the temp
    /// schema), which would otherwise carry over to the next session.
    reusable: bool,
    /// The engine continued from a previous session.
    reused: bool,
    /// Bytes of the schema's SQL as of the last commit, to weigh a parked
    /// engine; `None` when unknown, which prevents parking.
    schema_bytes: Option<usize>,
    /// SQL since the last commit may have changed the schema.
    schema_changed: bool,
    /// Holds the state's file for this session, dropped last.
    _open: Opened,
}

/// Every state's file an [`Engine`] runs on, so a second one is refused:
/// two would each keep a private view of the same file.
static OPENED: LazyLock<Mutex<HashSet<File>>> = LazyLock::new(Mutex::default);

/// A file in [`OPENED`], until dropped.
struct Opened(File);

impl Opened {
    fn new(file: File) -> Result<Self> {
        if !OPENED.lock().unwrap().insert(file.clone()) {
            return Err("a database already runs on this state; drop it first".into());
        }
        Ok(Self(file))
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        OPENED.lock().unwrap().remove(&self.0);
    }
}

/// The error of a session that a failure ended.
const ENDED: &str =
    "the session ended after a storage error, a cancelled call or a failed commit; open a new one";

/// An idle engine and its adapter at a flushed state, kept in the owner's
/// [`crate::state::PageCache`] so the next session can skip opening an engine. Only the
/// database is kept: each session connects afresh, so no connection state
/// (counters such as `last_insert_rowid()`, settings, attachments, the temp
/// schema) carries over.
struct WarmEngine {
    database: Arc<turso_core::Database>,
    io: Arc<KvIo>,
    /// Bytes of its schema's SQL, which sizes its parsed schema.
    schema_bytes: usize,
}

/// Memory of a parked engine besides its schema: its database's buffer pool,
/// which turso maps outside the allocator (a 3 MiB arena), and ~48 KiB more.
/// Measured as resident memory: ~4 MiB per engine parked.
const ENGINE_BYTES: usize = 4 << 20;

impl WarmEngine {
    /// Estimated memory: the buffer pool, and the parsed schema.
    fn weight(&self) -> usize {
        ENGINE_BYTES + 16 * self.schema_bytes
    }
}

/// Return to the executor once, even when every awaited future is ready.
pub(crate) async fn yield_now() {
    let mut yielded = false;
    poll_fn(move |cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

/// One session of a namespace's database, shared by the host's calls: every
/// SQL commit is durable when its call returns.
///
/// A call that leaves no SQL transaction open commits before it returns: what
/// the SQL committed, and the effects [`Self::commit`] set, in one flush of
/// the state. The host's `transaction()` runs its body between
/// [`Self::begin`] and [`Self::commit`] or [`Self::rollback`]; meanwhile a
/// statement that ends the transaction ends the session, rather than commit
/// the body halfway.
///
/// Calls hold the session one at a time; the host orders them (and keeps
/// other calls out of a transaction). A session ends with [`Self::save`],
/// which leaves a clean engine to the next session of the namespace,
/// [`Self::close`], or a failure; later calls fail.
pub struct Database {
    /// `None` once the session has ended.
    engine: futures::lock::Mutex<Option<Engine>>,
    state: State,
    /// A [`Self::transaction`] was dropped before it finished, its
    /// transaction maybe open: the next call ends the session.
    abandoned: AtomicBool,
    /// Ended by [`Self::save`] or [`Self::close`], rather than a failure.
    closed: AtomicBool,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: AtomicBool,
    /// The effects [`Self::commit`] set, for the call's flush to put in the
    /// same mutation as the SQL.
    effects: Mutex<Option<Bytes>>,
    /// The engine continued from a previous session.
    #[cfg(any(test, feature = "bench"))]
    reused: bool,
    /// The engine's adapter, for its I/O stats.
    #[cfg(any(test, feature = "bench"))]
    io: Arc<KvIo>,
}

impl Database {
    /// Open the database of `namespace` over `store`, kept warm in the
    /// process-wide page cache. A `reader` reads the namespace beside its
    /// owner (see [`crate::state::Open::reader`]): from the packs the owner
    /// cached, writing nothing to the store or the cache, it reads the state
    /// it opened on throughout.
    pub async fn open(store: Arc<dyn KvStore>, namespace: &str, reader: bool) -> Result<Self> {
        let open = State::open(store).cached(&page_cache().await?, namespace);
        Self::new(match reader {
            true => open.reader().await?,
            false => open.await?,
        })
        .await
    }

    /// A session of the database in `state`, however it was opened.
    pub async fn new(state: State) -> Result<Self> {
        Self::with_options(state, SqlOptions::default()).await
    }

    /// [`Self::new`] with explicit [`SqlOptions`].
    pub async fn with_options(state: State, options: SqlOptions) -> Result<Self> {
        let engine = Engine::open(&state, options).await?;
        Ok(Self {
            #[cfg(any(test, feature = "bench"))]
            reused: engine.reused,
            #[cfg(any(test, feature = "bench"))]
            io: engine.io.clone(),
            engine: futures::lock::Mutex::new(Some(engine)),
            state,
            abandoned: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            usable: AtomicBool::new(true),
            effects: Mutex::new(None),
        })
    }

    /// Run SQL (one or more statements; `params` bind to a single one) and
    /// return the column names and rows of its last statement; see
    /// [`Engine::query`]. Values never become SQL text, so they need no
    /// quoting.
    pub async fn query_columns(
        &self,
        sql: &str,
        params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        self.call(async |engine| engine.query(sql, params).await)
            .await
    }

    /// Run SQL and return the rows of its last statement.
    pub async fn query(&self, sql: &str) -> Result<Rows> {
        Ok(self.query_columns(sql, None).await?.1)
    }

    /// [`Self::query`] for one statement with its parameters (`?`, `?N`,
    /// `:name`, ...) bound in order.
    pub async fn query_with(
        &self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<Rows> {
        let params = params.into_iter().collect();
        Ok(self.query_columns(sql, Some(params)).await?.1)
    }

    /// Run SQL, discarding the rows.
    pub async fn execute(&self, sql: &str) -> Result<()> {
        self.query_columns(sql, None).await.map(drop)
    }

    /// [`Self::query_with`], discarding the rows.
    pub async fn execute_with(
        &self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<()> {
        self.query_with(sql, params).await.map(drop)
    }

    /// `BEGIN` the transaction of a `transaction()` body: until its `commit`
    /// or `rollback`, a statement that ends it ends the session.
    pub async fn begin(&self) -> Result<()> {
        self.call(async |engine| {
            if engine.in_body {
                return Err("transactions do not nest; use SAVEPOINT".into());
            }
            engine.execute("BEGIN").await?;
            engine.in_body = true;
            Ok(())
        })
        .await
    }

    /// `COMMIT` the transaction of a `transaction()` body; with `effects`,
    /// they commit with it, in the same flush, even if the transaction
    /// changed nothing. A `COMMIT` that fails sets no effects and leaves the
    /// transaction open, for `rollback`.
    pub async fn commit(&self, effects: Option<Bytes>) -> Result<()> {
        self.call(async |engine| {
            engine.in_body = false;
            // Nothing commits on a session a failure ended: the effects would
            // stay pending, and the call would resolve as if they committed.
            if !engine.is_usable() {
                return Err(ENDED.into());
            }
            if let Some(effects) = &effects {
                if self.state.is_reader() {
                    return Err("a reader commits no effects".into());
                }
                if effects.len() > crate::state::MAX_RECORD_BYTES {
                    let (size, limit) = (effects.len(), crate::state::MAX_RECORD_BYTES);
                    return Err(format!("effects of {size} bytes exceed {limit}").into());
                }
            }
            if engine.in_transaction() {
                engine.execute("COMMIT").await?;
            }
            *self.effects.lock().unwrap() = effects;
            Ok(())
        })
        .await
    }

    /// `ROLLBACK` the transaction of a `transaction()` body, if it is still
    /// open and no failure ended the session.
    pub async fn rollback(&self) -> Result<()> {
        self.call(async |engine| {
            engine.in_body = false;
            match engine.is_usable() && engine.in_transaction() {
                true => engine.execute("ROLLBACK").await,
                false => Ok(()),
            }
        })
        .await
    }

    /// Run `body` as one transaction, as the host's `transaction()` does:
    /// [`Self::begin`], then [`Self::commit`] when it returns `Ok`, or
    /// [`Self::rollback`] when it returns `Err` or the `COMMIT` fails.
    /// Dropped before it finishes, it ends the session: its transaction may
    /// be open, and later calls would silently join it.
    pub async fn transaction<T>(&self, body: impl AsyncFnOnce(&Self) -> Result<T>) -> Result<T> {
        self.begin().await?;
        let abandon = Abandon(self);
        let result = match body(self).await {
            Ok(value) => self.commit(None).await.map(|()| value),
            Err(error) => Err(error),
        };
        if result.is_err() {
            // The error says why: a failing rollback only ends the session.
            let _ = self.rollback().await;
        }
        std::mem::forget(abandon);
        result
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error or a cancelled call ends it. A call that is running
    /// does not change the answer until it ends.
    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them; empty if none did.
    pub async fn effects(&self) -> Result<Bytes> {
        let engine = self.engine.lock().await;
        engine.as_ref().ok_or(CLOSED)?;
        Ok(self.state.effects())
    }

    /// End the session, leaving its engine to the next one. It fails if a
    /// transaction was left open, whose changes are lost, or a failure ended
    /// the session.
    pub async fn save(&self) -> Result<()> {
        let engine = self.engine.lock().await.take();
        self.usable.store(false, Ordering::Release);
        let engine = match engine {
            Some(engine) => engine,
            None if self.closed.swap(true, Ordering::AcqRel) => return Err(CLOSED.into()),
            None => return Err(ENDED.into()),
        };
        self.closed.store(true, Ordering::Release);
        if !engine.is_usable() {
            return Err(ENDED.into());
        }
        if engine.in_transaction() {
            return Err("a transaction was left open: its changes are lost".into());
        }
        Ok(())
    }

    /// End the session, whatever state it is in.
    pub async fn close(&self) {
        self.engine.lock().await.take();
        self.closed.store(true, Ordering::Release);
        self.usable.store(false, Ordering::Release);
    }

    /// The state the database is kept in.
    #[cfg(any(test, feature = "bench"))]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Whether this session continued a previous session's engine.
    #[cfg(any(test, feature = "bench"))]
    pub fn reused_engine(&self) -> bool {
        self.reused
    }

    /// Local engine I/O only, useful for separating WAL traffic from backend writes.
    #[cfg(any(test, feature = "bench"))]
    pub fn io_stats(&self) -> IoStats {
        self.io.stats()
    }

    /// Checkpoint the state in a flush of its own, which rewrites the
    /// database's first page as it is: its pages are read from the
    /// checkpoint's pack after it. Nothing with an empty database.
    #[cfg(any(test, feature = "bench"))]
    pub async fn checkpoint(&self) -> Result<crate::state::FlushReport> {
        self.call(async |engine| {
            let mut mutation = engine.state.mutation();
            if let Some(first) = engine.state.file().read(0).await? {
                mutation.write(0, Some(first))?;
            }
            mutation.checkpoint();
            mutation.flush().await
        })
        .await
    }

    /// Run `call` on the open session, then settle it. A statement that
    /// ended the transaction of a `transaction()` body ends the session.
    /// Otherwise a call that leaves no SQL transaction open commits, even
    /// after an SQL error (the statements before it stand): what the SQL
    /// committed, with the effects set, in one flush, durable before the call
    /// returns. A reader commits nothing, and a session that a failure ended
    /// cannot. A call dropped before it returns ends the session.
    async fn call<T>(&self, call: impl AsyncFnOnce(&mut Engine) -> Result<T>) -> Result<T> {
        let mut running = Call {
            engine: self.engine.lock().await,
            usable: &self.usable,
            returned: false,
        };
        if self.abandoned.load(Ordering::Acquire) {
            running.engine.take();
            return Err(
                "a transaction() was dropped before it finished: the session has ended".into(),
            );
        }
        let Some(engine) = running.engine.as_mut() else {
            return Err(match self.closed.load(Ordering::Acquire) {
                true => CLOSED.into(),
                false => ENDED.into(),
            });
        };
        let result = call(engine).await;
        let effects = self.effects.lock().unwrap().take();
        if engine.in_body && !engine.in_transaction() {
            // `running` ends the session as it drops. A storage error that
            // ended the transaction too is the error to report.
            return match result {
                Err(error) if !engine.is_usable() => Err(error),
                _ => Err(BODY_ENDED.into()),
            };
        }
        let committed =
            match !self.state.is_reader() && engine.is_usable() && !engine.in_transaction() {
                true => commit(engine, effects).await,
                false => Ok(()),
            };
        self.usable.store(engine.is_usable(), Ordering::Release);
        running.returned = true;
        committed.and(result)
    }
}

/// A call holding the session: unless it returns, its state is unknown (a
/// cancelled statement, a commit whose outcome is unknown), and dropping
/// this ends the session.
struct Call<'a> {
    engine: MutexGuard<'a, Option<Engine>>,
    usable: &'a AtomicBool,
    returned: bool,
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        if !self.returned {
            self.usable.store(false, Ordering::Release);
            self.engine.take();
        }
    }
}

/// Marks its session's transaction abandoned when dropped: a
/// [`Database::transaction`] that did not finish.
struct Abandon<'a>(&'a Database);

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        self.0.abandoned.store(true, Ordering::Release);
        self.0.usable.store(false, Ordering::Release);
    }
}

/// The error of a call after the session ended.
const CLOSED: &str = "the database is closed";

/// The error of a statement that ended the transaction of a `transaction()`
/// body.
const BODY_ENDED: &str = "a statement ended the transaction of transaction(), whose body must not: the session has ended";

/// What the SQL committed, and `effects` if any, durable in one flush. A
/// flush the state refused leaves the pages private, for the next commit;
/// any other failure ends the state, and the engine with it.
async fn commit(engine: &mut Engine, effects: Option<Bytes>) -> Result<()> {
    let mut mutation = engine.state.mutation();
    if let Some(effects) = effects {
        mutation.set_effects(effects)?;
    }
    engine.flush(&mut mutation).await?;
    match mutation.flush().await {
        Ok(_) => {
            engine.io.settle_flushed();
            Ok(())
        }
        Err(error) => {
            engine.io.settle_discarded();
            Err(error)
        }
    }
}

/// The page cache shared by the process's sessions: 256 MiB of memory and
/// 1 GiB of disk, in a file under the temporary directory, or the file
/// `RESTATE_SQL_CACHE_FILE` names.
async fn page_cache() -> std::io::Result<PageCache> {
    static CACHE: futures::lock::Mutex<Option<PageCache>> = futures::lock::Mutex::new(None);
    let mut cache = CACHE.lock().await;
    if let Some(cache) = &*cache {
        return Ok(cache.clone());
    }
    let opened = PageCache::open(PageCacheConfig {
        file: std::env::var_os("RESTATE_SQL_CACHE_FILE").map(PathBuf::from),
        ..PageCacheConfig::new(256 << 20, 1 << 30)
    })
    .await?;
    Ok(cache.insert(opened).clone())
}

impl Engine {
    /// Open the engine over `state`'s file: one parked by this owner's
    /// previous session if the state found one, or a new one.
    async fn open(state: &State, options: SqlOptions) -> Result<Self> {
        let file = state.file();
        let opened = Opened::new(file.clone())?;
        let warm = state.take_parked::<WarmEngine>();
        let io = match &warm {
            Some(warm) => {
                warm.io.resume(file, options.max_private_bytes).await?;
                warm.io.clone()
            }
            None => KvIo::new(file, options.max_private_bytes),
        };
        let mut engine = Self {
            state: state.clone(),
            database: None,
            connection: None,
            io,
            max_result_bytes: options.max_result_bytes,
            poisoned: true,
            in_body: false,
            parked: false,
            reused: warm.is_some(),
            reusable: true,
            schema_bytes: warm.as_ref().map(|warm| warm.schema_bytes),
            schema_changed: false,
            _open: opened,
        };
        let database = match warm {
            Some(warm) => warm.database,
            None => {
                engine.io.prepare_engine().await?;
                open_engine(&engine.io).await?
            }
        };
        engine.connection = Some(connect(&engine.io, &database).await?);
        engine.database = Some(database);
        engine.poisoned = false;
        // A statement that would grow the database past the state's file cap
        // fails with SQLITE_FULL, and the session goes on.
        let page_size = engine.connection.as_ref().unwrap().get_page_size().get();
        let max_pages = crate::state::MAX_FILE_BYTES / u64::from(page_size);
        engine
            .internal(&format!("PRAGMA max_page_count = {max_pages}"))
            .await?;
        if !engine.reused && state.is_cached() {
            engine.schema_bytes = Some(engine.measure_schema().await?);
        }
        Ok(engine)
    }

    /// Leave an idle engine at a flushed state to the owner cache. Only an
    /// engine at the owner's latest acknowledged flush qualifies: one that an
    /// overlapping session left behind holds an older state.
    fn park(&mut self) {
        if !self.state.can_park() || !self.reusable || self.schema_changed {
            return;
        }
        let Some(schema_bytes) = self.schema_bytes else {
            return;
        };
        if self.is_poisoned() || self.in_transaction() || !self.io.is_clean() {
            return;
        }
        drop(self.connection.take());
        // Closing the connection must leave nothing behind.
        if !self.io.is_clean() {
            return;
        }
        if let Some(database) = self.database.take() {
            // An idle engine reads nothing until the next open gives it the
            // next state's file.
            self.io.release();
            let warm = WarmEngine {
                database,
                io: self.io.clone(),
                schema_bytes,
            };
            let weight = warm.weight();
            self.parked = self.state.park(warm, weight).is_ok();
        }
    }

    /// Run SQL (one or more statements; `params` bind to a single one) and
    /// return the column names and rows of its last statement, collected in
    /// memory, at most [`SqlOptions::max_result_bytes`] of them. A string of
    /// several statements runs them in order, each as if executed on its own.
    ///
    /// An SQL error (syntax, a constraint, a missing table) undoes that
    /// statement and leaves the engine usable; earlier statements of `sql`,
    /// and an open transaction, stand. Any other error or a cancellation ends
    /// it; see [`Self::is_usable`]. Cached execution yields every 64 steps;
    /// one engine call cannot be preempted. An engine helper requiring
    /// synchronous nonresident I/O returns an error.
    async fn query(
        &mut self,
        sql: &str,
        params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        self.screen(sql);
        self.run_statements(sql, params).await
    }

    async fn execute(&mut self, sql: &str) -> Result<()> {
        self.query(sql, None).await.map(drop)
    }

    /// Move what the SQL committed into the state's file, in `mutation`, for
    /// the mutation's flush to make durable. SQL goes on reading the moved
    /// pages from its private view until the caller settles them: the state
    /// holds them once the mutation flushed ([`KvIo::settle_flushed`]), and
    /// they are private again if it was refused ([`KvIo::settle_discarded`]).
    /// No SQL transaction may be open. A failure ends the engine, and may
    /// leave part of the move in the mutation: drop it.
    async fn flush(&mut self, mutation: &mut Mutation) -> Result<()> {
        if self.io.has_changes() || self.io.has_pending() {
            self.move_pages(mutation).await
        } else {
            self.measure_changed_schema().await
        }
    }

    /// Whether an SQL transaction is open.
    fn in_transaction(&self) -> bool {
        (self.connection.as_ref()).is_some_and(|connection| !connection.get_auto_commit())
    }

    /// Note what `sql` may change besides the database: connection state, which
    /// rules out reuse, and the schema, which a parked engine is weighed by.
    fn screen(&mut self, sql: &str) {
        if changes_connection_state(sql) {
            self.reusable = false;
        }
        if mentions(sql, &["create", "drop", "alter"]) {
            self.schema_changed = true;
        }
    }

    /// Bytes of the schema's SQL.
    async fn measure_schema(&mut self) -> Result<usize> {
        let rows = self
            .internal("SELECT coalesce(sum(length(sql)), 0) FROM sqlite_schema")
            .await?;
        let bytes = rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_int);
        Ok(bytes.unwrap_or(0).max(0) as usize)
    }

    /// Move the committed SQL state into the state's file, in `mutation`:
    /// its engine WAL into the file's pages. Nothing is durable yet.
    async fn move_pages(&mut self, mutation: &mut Mutation) -> Result<()> {
        self.checkpoint().await?;
        // Failing, the move leaves the mutation and the engine apart: the
        // engine ends.
        self.poisoned = true;
        self.io.flush_into(mutation)?;
        self.poisoned = false;
        self.measure_changed_schema().await
    }

    /// Weigh the schema again after a statement that may have changed it,
    /// for parking the engine; even one rolled back, which moved no pages.
    async fn measure_changed_schema(&mut self) -> Result<()> {
        if self.schema_changed && self.state.is_cached() {
            // Without a size the engine is just not parked.
            self.schema_bytes = self.measure_schema().await.ok();
            self.schema_changed = false;
        }
        Ok(())
    }

    /// Whether the engine can run more SQL: see [`Database::is_usable`].
    fn is_usable(&self) -> bool {
        !self.is_poisoned()
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned || !self.state.is_usable()
    }

    /// Run each statement of `sql`. With `params`, `sql` must be one
    /// statement.
    async fn run_statements(
        &mut self,
        sql: &str,
        mut params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        let mut remaining = sql;
        let mut rows = None;
        loop {
            if self.is_poisoned() {
                return Err(ENDED.into());
            }
            self.poisoned = true; // cancellation/error requires discarding the session
            let connection = self.connection.as_ref().unwrap();
            let prepared = match connection.consume_stmt(remaining) {
                Ok(prepared) => prepared,
                // A parse or name error: nothing ran.
                Err(error) if !self.io.failed() => {
                    self.poisoned = false;
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            };
            let Some((mut statement, end)) = prepared else {
                self.poisoned = false;
                break;
            };
            remaining = &remaining[end..];
            if let Some(values) = params.take() {
                // Refused before anything runs, so the session stays usable.
                match connection.consume_stmt(remaining) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        self.poisoned = false;
                        return Err("parameters bind to a single statement".into());
                    }
                    Err(error) => {
                        self.poisoned = false;
                        return Err(error.into());
                    }
                }
                if values.len() != statement.parameters_count() {
                    self.poisoned = false;
                    return Err(format!(
                        "the statement has {} parameters, {} values given",
                        statement.parameters_count(),
                        values.len()
                    )
                    .into());
                }
                for (index, value) in (1..).zip(values) {
                    if let Err(error) =
                        statement.bind_at(std::num::NonZero::new(index).unwrap(), value)
                    {
                        self.poisoned = false;
                        return Err(error.into());
                    }
                }
            }
            let limit = self.max_result_bytes;
            let columns = (0..statement.num_columns())
                .map(|i| statement.get_column_name(i).into_owned())
                .collect();
            let failure = match query_engine(&self.io, statement, limit).await {
                Ok(result) => {
                    rows = Some((columns, result));
                    None
                }
                Err(Failure::Sql(error)) => Some(error),
                Err(Failure::Fatal(error)) => return Err(error),
            };
            // Before the next statement of `sql`, which would run outside it.
            if self.in_body && connection.get_auto_commit() {
                return Err(BODY_ENDED.into());
            }
            self.poisoned = false;
            // The statement had no effect; earlier statements of `sql` stand.
            if let Some(error) = failure {
                return Err(error.into());
            }
        }
        rows.ok_or_else(|| "the SQL contains no statements".into())
    }

    /// Run one statement of this crate's own, outside the transaction rules.
    async fn internal(&mut self, sql: &str) -> Result<Rows> {
        if self.is_poisoned() {
            return Err(ENDED.into());
        }
        self.poisoned = true;
        let statement = self.connection.as_ref().unwrap().prepare(sql)?;
        let rows = match query_engine(&self.io, statement, usize::MAX).await {
            Ok(rows) => rows,
            Err(Failure::Sql(error)) => return Err(error.into()),
            Err(Failure::Fatal(error)) => return Err(error),
        };
        self.poisoned = false;
        Ok(rows)
    }

    /// Merge the private engine WAL into the private database overlay and truncate it.
    ///
    /// This is a local barrier, **not** backend durability. Failure poisons the
    /// engine. A successful checkpoint may still be discarded without saving.
    async fn checkpoint(&mut self) -> Result<()> {
        let rows = self.internal("PRAGMA wal_checkpoint(TRUNCATE)").await?;
        if rows.len() != 1
            || rows[0].len() != 3
            || rows[0][0].as_int() != Some(0)
            || rows[0][1] != rows[0][2]
        {
            self.poisoned = true;
            return Err("checkpoint did not complete fully".into());
        }
        if self.io.wal_len() != 0 {
            self.poisoned = true;
            return Err("checkpoint did not truncate the private WAL".into());
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // An engine that ends clean (every change flushed by the state)
        // leaves itself for the next session.
        self.park();
        if !self.parked {
            self.io.shutdown();
            drop(self.connection.take());
        }
    }
}

/// Whether SQL may change per-connection state rather than the database:
/// PRAGMA settings, ATTACH/DETACH or the temp schema. Conservative: a false
/// positive only costs reusing the engine.
fn changes_connection_state(sql: &str) -> bool {
    mentions(sql, &["pragma", "attach", "detach", "temp", "temporary"])
}

/// Whether `sql` contains one of `keywords` as a whole word, in any case.
fn mentions(sql: &str, keywords: &[&str]) -> bool {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| {
            keywords
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
}

/// Resume an engine completion only after the host has driven all submitted I/O.
async fn drive_completion(io: &KvIo, completion: turso_core::types::IOCompletions) -> Result<()> {
    if completion.is_explicit_yield() {
        yield_now().await;
    } else {
        io.drive().await?;
        if !completion.finished() {
            return Err("engine completion has no queued I/O".into());
        }
        if let Some(error) = completion.get_error() {
            return Err(turso_core::LimboError::from(error).into());
        }
    }
    Ok(())
}

async fn open_engine(io: &Arc<KvIo>) -> Result<Arc<turso_core::Database>> {
    let mounted: Arc<dyn IO> = Arc::new(MountedIo(io.clone()));
    let file = mounted.open_file(DB_PATH, OpenFlags::Create, false)?;
    let options =
        OpenOptions::new(Arc::new(SqliteDialect)).storage(Arc::new(DatabaseFile::new(file)));
    // Bypass the global registry: reopening constructs a cold engine.
    let mut state = turso_core::OpenDbAsyncState::new();
    let db = loop {
        match turso_core::Database::do_open_async(&mut state, mounted.clone(), DB_PATH, &options)? {
            IOResult::Done(db) => break db,
            IOResult::IO(completion) => drive_completion(io, completion).await?,
        }
    };
    Ok(db)
}

/// A new connection with this crate's settings.
async fn connect(io: &KvIo, db: &Arc<turso_core::Database>) -> Result<Arc<Connection>> {
    let mut state = ConnectAsyncState::default();
    let connection = loop {
        match db.connect_async(&mut state)? {
            IOResult::Done(connection) => break connection,
            IOResult::IO(completion) => drive_completion(io, completion).await?,
        }
    };
    connection.wal_auto_actions_disable();
    connection.set_temp_store(TempStore::Memory);
    Ok(connection)
}

/// Bytes a value takes in collected rows, approximately.
fn value_bytes(value: &Value) -> usize {
    16 + match value {
        Value::Text(text) => text.as_str().len(),
        Value::Blob(blob) => blob.len(),
        _ => 0,
    }
}

/// Why a statement failed.
enum Failure {
    /// The engine rejected the SQL (a constraint, a type error, ...) and undid
    /// the statement; no I/O failed and none is pending, so the session can go on.
    Sql(turso_core::LimboError),
    /// Anything else: storage, limits, anomalies. The session's state is unknown.
    Fatal(Box<dyn std::error::Error + Send + Sync>),
}

impl<E: Into<Box<dyn std::error::Error + Send + Sync>>> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure::Fatal(error.into())
    }
}

/// Abort queued requests before a suspended statement is destroyed. This guard
/// is created after the statement so cancellation retires completions first.
struct Running<'a> {
    io: &'a KvIo,
    complete: bool,
}
impl Drop for Running<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.io.shutdown();
        }
    }
}

/// Step `statement` to its end, driving its I/O and collecting its rows, at
/// most `max_bytes` of them.
async fn query_engine(
    io: &KvIo,
    mut statement: turso_core::Statement,
    max_bytes: usize,
) -> std::result::Result<Rows, Failure> {
    let mut running = Running {
        io,
        complete: false,
    };
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut steps = 0;
    loop {
        // Bound work between host yields even when all pages are already cached.
        // A single engine step is not preemptible by this adapter.
        if steps == 64 {
            yield_now().await;
            steps = 0;
        }
        steps += 1;
        let step = match statement.step() {
            Ok(step) => step,
            Err(error) if !io.failed() && io.pending_count() == 0 => {
                running.complete = true;
                return Err(Failure::Sql(error));
            }
            Err(error) => return Err(error.into()),
        };
        match step {
            StepResult::Row => {
                let row: Vec<Value> = statement
                    .row()
                    .ok_or("missing row")?
                    .get_values()
                    .cloned()
                    .collect();
                bytes += row.iter().map(value_bytes).sum::<usize>();
                if bytes > max_bytes {
                    return Err("result rows exceed SqlOptions::max_result_bytes".into());
                }
                rows.push(row);
            }
            StepResult::Done => {
                running.complete = true;
                return Ok(rows);
            }
            StepResult::IO => {
                let completion = statement
                    .take_io_completions()
                    .ok_or("engine yielded I/O without a completion")?;
                drive_completion(io, completion).await?;
            }
            StepResult::Yield => yield_now().await,
            StepResult::Busy | StepResult::Sleep { .. } => {
                return Err("unexpected contention in single-owner session".into());
            }
            StepResult::Interrupt => return Err("statement interrupted".into()),
        }
    }
}

#[cfg(test)]
#[path = "tests/unit/session.rs"]
mod checkpoint_tests;

#[cfg(test)]
mod tests;

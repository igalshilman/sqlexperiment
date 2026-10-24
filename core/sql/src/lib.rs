//! SQLite-compatible SQL on a [`State`]: Turso's Rust engine (`turso_core`,
//! unmodified) over the state's file `db`.
//!
//! For a Restate object without Restate's journal (`journalReplay: false`),
//! whose state is the namespace. [`Sql`] keeps its database in the state's
//! file `db`: its statements change a private view,
//! [`Sql::flush`] moves what SQL committed into a [`Mutation`], and the mutation's
//! [`Mutation::flush`] makes it durable, together with the mutation's other files and
//! records, atomically.
//!
//! # When SQL changes become durable
//!
//! At [`Mutation::flush`], and only then. SQL statements, SQL `COMMIT` and
//! [`Sql::checkpoint`] change a private view; [`Sql::flush`] moves what SQL
//! committed into a mutation, still private; a mutation flushed without it
//! leaves SQL's changes private. A mutation dropped discards what it
//! holds, and SQL keeps its pages for the next flush. The host decides when
//! to flush: after each SQL transaction (as SQLite does, and as the Node.js
//! addon and the Python extension do, through [`binding::Session`]), or once
//! per turn of everything that took part (as the durable functions runtime
//! does).
//!
//! | Layer | Representation | Survives dropping it? |
//! |---|---|---|
//! | SQL working state | Dirty 4096-byte pages and the private engine WAL | No |
//! | A mutation of the state | The pages SQL flushed into it, and its records | No |
//! | The state | Its root and blobs (see [`state`]) | Yes, after an acknowledged flush |
//!
//! An SQL error (syntax, a constraint, a missing table) undoes just that
//! statement, as in SQLite, and the session goes on. A storage error, a
//! cancelled call or a failed flush ends the session ([`Sql::is_usable`]
//! returns false): settle outstanding requests and open a new one.
//!
//! # Engine reuse
//!
//! With a [`state::PageCache`], a session that ends cleanly leaves its idle
//! engine there ([`State::park`]); the next session of the
//! namespace reuses it when nothing changed, keeping the parsed schema and the
//! engine's page cache.
//!
//! # Driving the engine
//!
//! The caller's task drives nonblocking engine open/connect state machines and
//! statement steps, awaits queued state reads, then resumes the engine.
//! Completions run on that task. No worker thread, command channel or
//! background task is created. Cached queries yield to the executor after 64
//! steps; an individual engine call is not preemptible. Queries on a session
//! remain sequential.
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
//! ```no_run
//! # use std::sync::Arc;
//! # use state::{State, kv::KvStore};
//! # use sql::Sql;
//! # async fn example(store: Arc<dyn KvStore>) -> state::Result<()> {
//! let state = State::open(store).await?;
//! let mut sql = Sql::open(&state).await?;
//! sql.execute("CREATE TABLE messages(body TEXT)").await?;
//! sql.execute("INSERT INTO messages VALUES('hello')").await?;
//! let mut mutation = state.mutation();
//! sql.flush(&mut mutation).await?;
//! mutation.set(&state.record("seen"), "1 message".into())?;
//! mutation.flush().await?; // the table and the record, durable together
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
//! `simulation` turns on the state crate's in-memory store, fault injection
//! and latency simulator, and this crate's test helpers.
pub mod binding;
pub mod io;

/// SQL values, as returned in rows and bound as parameters.
pub use turso_core::{Numeric, Value};

use crate::io::{DB_PATH, KvIo, MountedIo};
use state::{File, Mutation, Receipt, State};
use std::{
    future::poll_fn,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};
use turso_core::storage::database::DatabaseFile;
use turso_core::{
    ConnectAsyncState, Connection, Database, IO, IOResult, OpenFlags, OpenOptions, SqliteDialect,
    StepResult, TempStore,
};

/// The state's file SQL keeps its database in.
pub const DB_FILE: &str = "db";

/// The key SQL parks its engine under ([`State::park`]).
const ENGINE_KEY: &str = "sql";

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
#[cfg(feature = "simulation")]
#[derive(Clone, Copy, Debug)]
pub struct IoStats {
    pub database_bytes_written: u64,
    pub private_wal_bytes_written: u64,
    pub database_len: u64,
    pub private_wal_len: u64,
}

/// Adapter, engine and state errors returned to the host task.
pub type Result<T> = state::Result<T>;
/// Result rows, each a value per column.
pub type Rows = Vec<Vec<Value>>;

/// The SQL engine over a state's file `db`: one private SQL view, driven
/// entirely by the caller's async task.
///
/// The host advances engine state machines, awaits queued KV I/O and resumes the
/// engine on the same task. No worker thread or background task is created.
/// Queries require exclusive access to this handle. An SQL error leaves the
/// session usable; a storage error, a cancelled call or a failed flush ends
/// it (see [`Self::is_usable`]): open a new one after settling backend requests.
/// Its changes become durable only with a mutation's [`Mutation::flush`], after
/// [`Self::flush`] into it; dropping it leaves its engine to the next one.
pub struct Sql {
    state: State,
    database: Option<Arc<Database>>,
    connection: Option<Arc<Connection>>,
    io: Arc<KvIo>,
    /// For the mutation the pages were last flushed into, until the state flushes
    /// or drops it (see [`Self::settle`]).
    receipt: Option<Receipt>,
    /// See [`SqlOptions::max_result_bytes`].
    max_result_bytes: usize,
    poisoned: bool,
    /// The engine moved into the owner cache; nothing is left to shut down.
    parked: bool,
    /// No SQL changed per-connection state (settings, attachments, the temp
    /// schema), which would otherwise carry over to the next session.
    reusable: bool,
    /// The engine continued from a previous session.
    reused: bool,
    /// A [`Self::transaction`] body is running.
    in_body: bool,
    /// Set when a [`Self::transaction`] future is dropped or unwinds before
    /// finishing, which may leave its SQL transaction open.
    abandoned: Arc<AtomicBool>,
    /// Bytes of the schema's SQL as of the last commit, to weigh a parked
    /// engine; `None` when unknown, which prevents parking.
    schema_bytes: Option<usize>,
    /// SQL since the last commit may have changed the schema.
    schema_changed: bool,
    /// Holds the state's file `db` for this session, dropped last.
    _open: Opened,
}

/// Every state's file `db` an [`Sql`] runs on, so a second one is refused:
/// two would each keep a private view of the same file.
static OPENED: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// A file in [`OPENED`], until dropped.
struct Opened(File);

impl Opened {
    fn new(file: File) -> Result<Self> {
        let mut opened = OPENED.lock().unwrap();
        if opened.contains(&file) {
            return Err("an Sql already runs on this state; drop it first".into());
        }
        opened.push(file.clone());
        Ok(Self(file))
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        OPENED.lock().unwrap().retain(|file| *file != self.0);
    }
}

/// The error of a session that a failure ended.
const ENDED: &str =
    "the session ended after a storage error, a cancelled call or a failed commit; open a new one";

/// Marks a session abandoned unless it finished; see [`Sql::transaction`].
struct Abandon {
    abandoned: Arc<AtomicBool>,
    finished: bool,
}
impl Drop for Abandon {
    fn drop(&mut self) {
        if !self.finished {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }
}

/// An idle engine and its adapter at a flushed state, kept in the owner's
/// [`state::PageCache`] so the next session can skip opening an engine. Only the
/// database is kept: each session connects afresh, so no connection state
/// (counters such as `last_insert_rowid()`, settings, attachments, the temp
/// schema) carries over.
struct WarmEngine {
    database: Arc<Database>,
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

impl Sql {
    /// Local engine I/O only, useful for separating WAL traffic from backend writes.
    #[cfg(feature = "simulation")]
    pub fn io_stats(&self) -> IoStats {
        self.io.stats()
    }

    /// Open the SQL database of `state`, its file `db`, with default
    /// [`SqlOptions`]: an engine parked by this owner's previous session
    /// if the state found one, or a new one. One `Sql` runs on a state at a
    /// time.
    pub async fn open(state: &State) -> Result<Self> {
        Self::open_with_options(state, SqlOptions::default()).await
    }

    /// [`Self::open`] with explicit [`SqlOptions`].
    pub async fn open_with_options(state: &State, options: SqlOptions) -> Result<Self> {
        let file = state.file(DB_FILE)?;
        let opened = Opened::new(file.clone())?;
        let warm = state.take_parked::<WarmEngine>(ENGINE_KEY);
        let io = match &warm {
            Some(warm) => {
                warm.io.resume(file, options.max_private_bytes).await?;
                warm.io.clone()
            }
            None => KvIo::new(file, options.max_private_bytes),
        };
        let mut sql = Self {
            state: state.clone(),
            database: None,
            connection: None,
            io,
            receipt: None,
            max_result_bytes: options.max_result_bytes,
            poisoned: true,
            parked: false,
            reused: warm.is_some(),
            reusable: true,
            in_body: false,
            abandoned: Arc::default(),
            schema_bytes: warm.as_ref().map(|warm| warm.schema_bytes),
            schema_changed: false,
            _open: opened,
        };
        let database = match warm {
            Some(warm) => warm.database,
            None => {
                sql.io.prepare_engine().await?;
                open_engine(&sql.io).await?
            }
        };
        sql.connection = Some(connect(&sql.io, &database).await?);
        sql.database = Some(database);
        sql.poisoned = false;
        // A statement that would grow the database past the state's file cap
        // fails with SQLITE_FULL, and the session goes on.
        let page_size = sql.connection.as_ref().unwrap().get_page_size().get();
        let max_pages = state::MAX_FILE_BYTES / u64::from(page_size);
        sql.internal(&format!("PRAGMA max_page_count = {max_pages}"))
            .await?;
        if !sql.reused && state.is_cached() {
            sql.schema_bytes = Some(sql.measure_schema().await?);
        }
        Ok(sql)
    }

    /// The state this SQL flushes with.
    pub fn state(&self) -> State {
        self.state.clone()
    }

    /// Whether this session continued a previous session's engine.
    #[cfg(feature = "simulation")]
    pub fn reused_engine(&self) -> bool {
        self.reused
    }

    /// Leave an idle engine at a flushed state to the owner cache. Only an
    /// engine at the owner's latest acknowledged flush qualifies: one that an
    /// overlapping session left behind holds an older state.
    fn park(&mut self) {
        self.settle();
        if !self.state.can_park() || !self.reusable || self.schema_changed {
            return;
        }
        let Some(schema_bytes) = self.schema_bytes else {
            return;
        };
        let idle = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.get_auto_commit());
        if self.is_poisoned() || !idle || !self.io.is_clean() {
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
            self.parked = self.state.park(ENGINE_KEY, warm, weight).is_ok();
        }
    }

    /// Execute SQL and collect the result rows of its last statement in memory.
    /// A string of several statements runs them in order, each as if executed
    /// on its own.
    ///
    /// There is no streaming cursor: the rows are limited to
    /// [`SqlOptions::max_result_bytes`]. SQL writes remain private until
    /// [`Self::flush`] and the mutation's flush. An SQL error (syntax, a constraint, a
    /// missing table) undoes that statement and leaves the session usable;
    /// earlier statements of `sql`, and an open transaction, stand. Any other
    /// error or a cancellation ends the session, and a failed flush's outcome
    /// is unknown; see [`Self::is_usable`].
    /// Cached execution yields every 64 steps; one engine call cannot be preempted.
    /// An engine helper requiring synchronous nonresident I/O returns an error.
    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        self.run(sql, None).await.map(|(_, rows)| rows)
    }

    /// [`Self::query`] for one statement with its parameters (`?`, `?N`,
    /// `:name`, ...) bound in order. Values never become SQL text, so they need
    /// no quoting. The number of values must match the statement's parameters.
    pub async fn query_with(
        &mut self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<Rows> {
        let params = params.into_iter().collect();
        self.run(sql, Some(params)).await.map(|(_, rows)| rows)
    }

    /// [`Self::query`] (without `params`) or [`Self::query_with`], also
    /// returning the column names of the statement whose rows these are.
    pub async fn query_columns(
        &mut self,
        sql: &str,
        params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        self.run(sql, params).await
    }

    /// Execute SQL, discarding the result rows; see [`Self::query`].
    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        self.query(sql).await.map(|_| ())
    }

    /// [`Self::query_with`], discarding the result rows.
    pub async fn execute_with(
        &mut self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<()> {
        self.query_with(sql, params).await.map(|_| ())
    }

    /// `COMMIT` the open transaction, if any, and move what the SQL committed
    /// into the state's file `db`, in `mutation`, for the mutation's flush to make
    /// durable. SQL goes on reading the moved pages from its private view
    /// until the state flushes the mutation (its [`Receipt`] tells); a mutation dropped
    /// instead leaves them for the next flush.
    ///
    /// A `COMMIT` that fails with an SQL error (a deferred constraint) changes
    /// nothing: the transaction stays open, for the caller to roll back. Any
    /// other failure ends the session, and may leave part of the move in the
    /// mutation: drop it.
    pub async fn flush(&mut self, mutation: &mut Mutation) -> Result<()> {
        self.settle();
        let result = match self.in_transaction() {
            true => self.execute("COMMIT").await,
            false => Ok(()),
        };
        match result {
            // Pages a pending mutation holds go in too: it may be dropped.
            Ok(()) if self.io.has_changes() || self.io.has_pending() => {
                self.move_pages(mutation).await
            }
            Ok(()) => self.measure_changed_schema().await,
            result => result,
        }
    }

    /// Act on what became of the mutation the pages were last moved into: flushed
    /// by the state, the file holds them now; dropped, they are private
    /// changes again.
    fn settle(&mut self) {
        let Some(receipt) = &self.receipt else {
            return;
        };
        if receipt.is_flushed() {
            self.io.settle_flushed();
        } else if receipt.is_discarded() {
            self.io.settle_discarded();
        } else {
            return;
        }
        self.receipt = None;
    }

    /// Whether an SQL transaction is open.
    pub fn in_transaction(&self) -> bool {
        (self.connection.as_ref()).is_some_and(|connection| !connection.get_auto_commit())
    }

    /// Run `body` as one SQL transaction: `BEGIN`, then `COMMIT` when it returns
    /// `Ok`, or `ROLLBACK` when it returns `Err`: committed in the SQL, for
    /// a [`Self::flush`] and the mutation's flush to make durable.
    ///
    /// The body may run any statements except ones that end the transaction
    /// (`COMMIT`, `END`, `ROLLBACK`): those fail and poison the session before
    /// anything becomes durable. Dropping the future or unwinding before it
    /// finishes also poisons the session, since its transaction may be open.
    pub async fn transaction<T>(
        &mut self,
        body: impl AsyncFnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if self.in_body {
            return Err("transactions do not nest; use SAVEPOINT".into());
        }
        let mut guard = Abandon {
            abandoned: self.abandoned.clone(),
            finished: false,
        };
        if let Err(error) = self.execute("BEGIN").await {
            // No transaction began: an SQL error (such as one already open)
            // leaves the session usable, and a storage error has ended it.
            guard.finished = true;
            return Err(error);
        }
        self.in_body = true;
        let result = body(self).await;
        self.in_body = false;
        let open = !self.is_poisoned() && !self.connection.as_ref().unwrap().get_auto_commit();
        let result = match result {
            Ok(value) if open => match self.execute("COMMIT").await {
                Ok(()) => Ok(value),
                // A COMMIT that fails (a deferred constraint) leaves the
                // transaction open: without the rollback, the statements
                // after it would run inside it.
                Err(error) => self.rollback(error).await,
            },
            Ok(_) => Err(ENDED.into()),
            Err(error) if open => self.rollback(error).await,
            Err(error) => Err(error),
        };
        guard.finished = true;
        result
    }

    /// Roll back the open transaction of [`Self::transaction`], which fails
    /// with `error`: a failing rollback does not replace it, but ends the
    /// session if the transaction is still open.
    async fn rollback<T>(&mut self, error: Box<dyn std::error::Error + Send + Sync>) -> Result<T> {
        if !self.is_poisoned()
            && !self.connection.as_ref().unwrap().get_auto_commit()
            && self.execute("ROLLBACK").await.is_err()
            && !self.connection.as_ref().unwrap().get_auto_commit()
        {
            self.poisoned = true;
        }
        Err(error)
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

    /// Move the committed SQL state into the state's file `db`, in `mutation`:
    /// its engine WAL into the file's pages. Nothing is durable yet.
    async fn move_pages(&mut self, mutation: &mut Mutation) -> Result<()> {
        self.checkpoint().await?;
        // Failing, the move leaves the mutation and the engine apart: the session
        // ends.
        self.poisoned = true;
        self.io.flush_into(mutation)?;
        self.receipt = Some(mutation.receipt());
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

    /// The state's version: the flushes ever made, as this session holds the
    /// state (opened on, or flushed itself); SQL not yet flushed by the state
    /// does not change it.
    pub fn version(&self) -> u64 {
        self.state.version()
    }

    /// Whether the session can run more SQL. An SQL error (a syntax error, a
    /// constraint, a missing table) leaves it usable: the failed statement had
    /// no effect, and an open transaction continues. A storage error, a
    /// cancelled call, a failed flush or an exceeded limit ends it: discard it
    /// and open a new one, after settling outstanding requests.
    pub fn is_usable(&self) -> bool {
        !self.is_poisoned()
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned || self.abandoned.load(Ordering::SeqCst) || !self.state.is_usable()
    }

    /// Run the caller's `sql` (see [`Self::run_statements`]), returning the
    /// column names and rows of its last statement.
    async fn run(&mut self, sql: &str, params: Option<Vec<Value>>) -> Result<(Vec<String>, Rows)> {
        self.settle();
        self.screen(sql);
        self.run_statements(sql, params).await
    }

    /// Run each statement of `sql`, applying the transaction rules after each:
    /// inside a [`Self::transaction`] body the transaction must stay open.
    /// With `params`, `sql` must be one statement.
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
            let idle = connection.get_auto_commit();
            if self.in_body && idle {
                return Err("a statement ended the transaction of Sql::transaction".into());
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
        self.settle();
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
    /// session. A successful checkpoint may still be discarded without saving.
    pub async fn checkpoint(&mut self) -> Result<()> {
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

impl Drop for Sql {
    fn drop(&mut self) {
        // A session that ends clean (every change flushed by the state)
        // leaves its engine for the next one.
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

async fn open_engine(io: &Arc<KvIo>) -> Result<Arc<Database>> {
    let mounted: Arc<dyn IO> = Arc::new(MountedIo(io.clone()));
    let file = mounted.open_file(DB_PATH, OpenFlags::Create, false)?;
    let options =
        OpenOptions::new(Arc::new(SqliteDialect)).storage(Arc::new(DatabaseFile::new(file)));
    // Bypass the global registry: reopening constructs a cold engine.
    let mut state = turso_core::OpenDbAsyncState::new();
    let db = loop {
        match Database::do_open_async(&mut state, mounted.clone(), DB_PATH, &options)? {
            IOResult::Done(db) => break db,
            IOResult::IO(completion) => drive_completion(io, completion).await?,
        }
    };
    Ok(db)
}

/// A new connection with this crate's settings.
async fn connect(io: &KvIo, db: &Arc<Database>) -> Result<Arc<Connection>> {
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

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/session.rs"]
mod checkpoint_tests;

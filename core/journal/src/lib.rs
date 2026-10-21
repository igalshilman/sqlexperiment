//! A durable journal over an asynchronous key/value store: paged files and
//! records, committed together in one put, with SQLite-compatible SQL on the
//! file `db`.
//!
//! A [`Journal`] is one namespace in a KV store: a Restate object's state,
//! which keeps writes in issue order and makes them durable at a sync (see
//! [`kv`]). It needs no multi-key transaction, compare-and-swap, listing
//! operation or backend WAL API. Its [`File`]s are 4 KiB pages read and
//! written at random, and its [`Record`]s whole values in the commit record;
//! [`Journal::commit`] makes every change durable at once. [`Sql`] runs
//! Turso's Rust engine (`turso_core`) on the file `db`.
//!
//! # When changes become durable
//!
//! At [`Journal::commit`], and only then: the journal's one put makes every
//! record set, file written and SQL change flushed since the last commit
//! durable together. SQL statements, SQL `COMMIT` and [`Sql::checkpoint`]
//! change a private view; [`Sql::flush`] moves what SQL committed into the
//! file `db`, still private; dropping the journal discards it all. The host
//! decides when to commit: after each SQL transaction (as SQLite does), or
//! once per turn of everything that took part.
//!
//! | Layer | Representation | Survives dropping the session? |
//! |---|---|---|
//! | SQL working state | Dirty 4096-byte pages and private engine WAL | No |
//! | Commit record | `commit`: the log since the last checkpoint, one transaction per save, compressed against the previous images; a large one's segments at `log/<n>` | Yes, after an acknowledged save |
//! | Pages | `page/<file>/<n>`, overwritten in place by checkpoints | Yes |
//!
//! A save commits by putting the record with one more transaction, usually a
//! single put, then one sync. An unchanged save performs no mutations. An
//! error or cancellation can have an unknown commit outcome. Settle
//! outstanding backend requests and reopen before deciding whether to retry
//! application work.
//!
//! # Why a log and pages in place?
//!
//! One put is the only atomic operation the backend offers: a crash may keep
//! some of a save's writes and lose the ones after them. The record turns one
//! put into a multi-page commit: a transaction is visible once a record
//! holding it is. Because the log holds every page a checkpoint has not yet
//! recorded, checkpoints may overwrite `page/<n>` in place, after the record:
//! replay applies the log over whatever they left. This is SQLite's own WAL design, applied to KV keys.
//! There is no page map, no object allocation, no intent and no orphan collection.
//! See `docs/DESIGN.md` and the private `snapshot` and `log` modules.
//!
//! The costs are writing each page twice (log, then checkpoint) and replaying the
//! log on open. A logged page costs about what changed in it since its previous
//! image, so checkpoints can be rare. This is not MVCC: one owner reads
//! and writes the namespace.
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including settlement
//! of cancelled requests before reopening or handing ownership to another session.
//! There is no distributed lock or fence here. A save queues its commit, then,
//! when the log is long, its checkpoint, and syncs once. Open reads the
//! record, and the stored segments its log names, if any. The I/O pump
//! batches adjacent file reads without crossing write, truncate or sync
//! barriers.
//!
//! Sessions of one owner may overlap when they share a [`NamespaceCache`]:
//! each reads the state it opened at (a checkpoint waits while another session
//! holds an older version), and a commit is refused if another
//! session committed since this one opened. Without a cache they must not
//! overlap.
//!
//! [`StorageOptions::cache`] can hold a [`NamespaceCache`]: one namespace's handle
//! on the process-wide [`PageCache`], which keeps pages in memory and on a local
//! disk tier under one budget for every namespace. It keeps them while the
//! record's epoch (its checkpoint count) is the one they belong to, and
//! remembers the record this owner made durable, so a steady-state open and
//! save are each one round trip. A session that ends cleanly leaves its idle engine
//! there; the next open reuses it when nothing changed, keeping the parsed
//! schema and engine page cache. The disk tier's I/O runs on tokio.
//!
//! The caller's task drives nonblocking engine open/connect state machines and
//! statement steps, awaits queued KV work, then resumes the engine. Completions run
//! on that task. No worker thread, command channel or background task is created,
//! apart from the disk tier of a [`PageCache`].
//! Cached queries yield to the executor after 64 steps; an individual engine call
//! is not preemptible. Queries on a session remain sequential.
//!
//! The pinned engine also has synchronous metadata helpers. Open asynchronously
//! retains page zero for these reads; dirty overlay bytes always take priority.
//! `IO::step` serves only resident reads and local sync barriers. A synchronous
//! nonresident read or file mutation fails explicitly, without starting backend I/O.
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
//! # use journal::{Journal, Sql, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> journal::Result<()> {
//! let journal = Journal::open(store).await?;
//! let mut sql = Sql::open(&journal).await?;
//! sql.execute("CREATE TABLE messages(body TEXT)").await?;
//! sql.execute("INSERT INTO messages VALUES('hello')").await?;
//! sql.flush().await?;
//! journal.record("seen").set("1 message".into());
//! journal.commit().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! Log segments are capped at [`MAX_SEGMENT_BYTES`]; larger saves span several.
//! A record's value is capped at [`MAX_RECORD_BYTES`], its name at
//! [`MAX_NAME_BYTES`].
//! A session's private changes and a statement's result rows are capped by
//! [`StorageOptions::max_private_bytes`] and [`StorageOptions::max_result_bytes`].
//! The backend is assumed to return what was stored; decoding checks structure
//! and bounds, not checksums. Use a fresh, dedicated namespace; no migration or format discovery is
//! implemented. Simulation and benchmark support require the opt-in `simulation`
//! feature.
mod cache;
mod format;
mod hint;
pub mod io;
pub mod kv;
mod log;
pub mod page_codec;
#[cfg(feature = "simulation")]
pub mod simulation;
mod snapshot;

pub use cache::{NamespaceCache, PageCache, PageCacheConfig};
pub use hint::MAX_HINT_PAGES;
pub use log::{Base, MAX_NAME_BYTES, MAX_RECORD_BYTES, MAX_SEGMENT_BYTES};
pub use page_codec::PageCompression;
pub use snapshot::{
    CHECKPOINT_AFTER, CHECKPOINT_LOG_BYTES, INLINE_LOG_BYTES, MAX_PRIVATE_BYTES, MAX_RESULT_BYTES,
    SaveReport, StorageOptions,
};
/// SQL values, as returned in rows and bound as parameters.
pub use turso_core::{Numeric, Value};

use crate::io::{DB_PATH, KvIo, MountedIo};
use crate::kv::KvStore;
use crate::page_codec::Page;
use crate::snapshot::Snapshot;
use bytes::Bytes;
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

/// Adapter, engine and backend errors returned to the host task.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Rows = Vec<Vec<Value>>;

/// The SQL engine over a journal's file `db`: one private SQL view, driven
/// entirely by the caller's async task.
///
/// The host advances engine state machines, awaits queued KV I/O and resumes the
/// engine on the same task. No worker thread or background task is created.
/// Queries require exclusive access to this handle. An SQL error leaves the
/// session usable; a storage error, a cancelled call or a failed commit ends
/// it (see [`Self::is_usable`]): open a new one after settling backend requests.
/// Its changes become durable only with the journal's [`Journal::commit`],
/// after [`Self::flush`]; dropping it leaves its engine to the next one.
pub struct Sql {
    journal: Journal,
    database: Option<Arc<Database>>,
    connection: Option<Arc<Connection>>,
    io: Arc<KvIo>,
    poisoned: bool,
    /// The engine moved into the owner cache; nothing is left to shut down.
    parked: bool,
    /// No SQL changed per-connection state (settings, attachments, the temp
    /// schema), which would otherwise carry over to the next session.
    reusable: bool,
    /// The engine continued from a previous session.
    #[cfg_attr(not(feature = "simulation"), allow(dead_code))]
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
}

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

/// An idle engine and its adapter at a committed state, kept in the owner's
/// [`PageCache`] so the next session can skip opening an engine. Only the
/// database is kept: each session connects afresh, so no connection state
/// (counters such as `last_insert_rowid()`, settings, attachments, the temp
/// schema) carries over.
pub(crate) struct WarmEngine {
    database: Arc<Database>,
    io: Arc<KvIo>,
    /// The version its state corresponds to.
    version: u64,
    /// Bytes of its schema's SQL, which sizes its parsed schema.
    schema_bytes: usize,
}

/// Memory of a parked engine besides its schema: its database's buffer pool,
/// which turso maps outside the allocator (a 3 MiB arena), and ~48 KiB more.
/// Measured as resident memory: ~4 MiB per engine parked.
const ENGINE_BYTES: usize = 4 << 20;

impl WarmEngine {
    /// Estimated memory: the buffer pool, and the parsed schema.
    pub(crate) fn weight(&self) -> usize {
        ENGINE_BYTES + 16 * self.schema_bytes
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
    pub fn io_stats(&self) -> crate::simulation::IoStats {
        self.io.stats()
    }

    /// Open the SQL database of `journal`, its file `db`: an engine left idle
    /// by this owner's previous session if the journal found one, or a new
    /// one. One `Sql` runs on a journal.
    pub async fn open(journal: &Journal) -> Result<Self> {
        let shared = &journal.shared;
        if shared.sql.swap(true, Ordering::SeqCst) {
            return Err("an Sql already runs on this journal".into());
        }
        let warm = shared.warm.lock().unwrap().take();
        let mut sql = Self {
            journal: Journal {
                shared: shared.clone(),
                prefix: Arc::from(""),
            },
            database: None,
            connection: None,
            io: shared.io.clone(),
            poisoned: true,
            parked: false,
            reused: warm.is_some(),
            reusable: true,
            in_body: false,
            abandoned: Arc::default(),
            schema_bytes: warm.as_ref().map(|warm| warm.schema_bytes),
            schema_changed: false,
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
        if !sql.reused && shared.options.cache.is_some() {
            sql.schema_bytes = Some(sql.measure_schema().await?);
        }
        Ok(sql)
    }

    /// The journal this SQL commits with.
    pub fn journal(&self) -> Journal {
        self.journal.clone()
    }

    /// Whether this session continued a previous session's engine.
    #[cfg(feature = "simulation")]
    pub fn reused_engine(&self) -> bool {
        self.reused
    }

    /// Leave an idle engine at a committed state to the owner cache. Only an
    /// engine at the owner's latest acknowledged commit qualifies: one that an
    /// overlapping session left behind holds an older state.
    fn park(&mut self) {
        let Some(cache) = &self.journal.shared.options.cache else {
            return;
        };
        let version = self.io.version();
        if cache.committed() != Some(version) || !self.reusable || self.schema_changed {
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
            // An idle engine reads nothing: it holds back no checkpoint, and the
            // next open replaces its view.
            self.io.idle();
            cache.park(WarmEngine {
                database,
                io: self.io.clone(),
                version,
                schema_bytes,
            });
            self.parked = true;
        }
    }

    /// Execute SQL and collect the result rows of its last statement in memory.
    /// A string of several statements runs them in order, each as if executed
    /// on its own.
    ///
    /// There is no streaming cursor: the rows are limited to
    /// [`StorageOptions::max_result_bytes`]. With
    /// SQL writes remain private until [`Self::flush`] and the journal's
    /// commit; a statement that ends a write transaction returns
    /// once that transaction is durable. An SQL error (syntax, a constraint, a
    /// missing table) undoes that statement and leaves the session usable;
    /// earlier statements of `sql`, and an open transaction, stand. Any other
    /// error or a cancellation ends the session, and a failed commit's outcome
    /// is unknown; see [`Self::is_usable`].
    /// Cached execution yields every 64 steps; one engine call cannot be preempted.
    /// An engine helper requiring synchronous nonresident I/O returns an error.
    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        self.screen(sql);
        self.run(sql, None).await
    }

    /// [`Self::query`] for one statement with its parameters (`?`, `?N`,
    /// `:name`, ...) bound in order. Values never become SQL text, so they need
    /// no quoting. The number of values must match the statement's parameters.
    pub async fn query_with(
        &mut self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<Rows> {
        self.screen(sql);
        self.run(sql, Some(params.into_iter().collect())).await
    }

    /// [`Self::query`] (without `params`) or [`Self::query_with`], also
    /// returning the column names of the statement whose rows these are.
    pub async fn query_columns(
        &mut self,
        sql: &str,
        params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        self.screen(sql);
        self.run_columns(sql, params).await
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
    /// into the journal's file `db`, for the journal's next commit to make
    /// durable. A `COMMIT` that fails changes nothing, and discards what the
    /// journal holds since its last commit.
    pub async fn flush(&mut self) -> Result<()> {
        let open = self.in_transaction();
        let result = match open {
            true => self.execute("COMMIT").await,
            false => Ok(()),
        };
        let result = match result {
            Ok(()) if self.io.wal_len() != 0 => self.move_pages().await,
            result => result,
        };
        if result.is_err() {
            self.journal.discard();
        }
        result
    }

    /// Whether an SQL transaction is open.
    pub fn in_transaction(&self) -> bool {
        (self.connection.as_ref()).is_some_and(|connection| !connection.get_auto_commit())
    }

    /// Run `body` as one SQL transaction: `BEGIN`, then `COMMIT` when it returns
    /// `Ok`, or `ROLLBACK` when it returns `Err`: committed in the SQL, for
    /// a [`Self::flush`] and the journal's commit to make durable.
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

    /// Move the committed SQL state into the journal's file `db`: its WAL
    /// into the file's pages. Nothing is durable yet.
    async fn move_pages(&mut self) -> Result<()> {
        self.checkpoint().await?;
        if self.schema_changed && self.journal.shared.options.cache.is_some() {
            // Without a size the engine is just not parked.
            self.schema_bytes = self.measure_schema().await.ok();
            self.schema_changed = false;
        }
        Ok(())
    }

    /// The database's version: the transactions ever committed, as this
    /// session holds the database (opened on, or committed itself); an open
    /// transaction does not change it.
    pub fn version(&self) -> u64 {
        self.journal.version()
    }

    /// Whether the store still holds the state this session opened on (see
    /// [`Self::open_reader`]): if it does not, what the session read may mix
    /// that state with a later checkpoint's pages, so read again in a new
    /// session. A session's own commits change the state, so this is for a
    /// session that has not committed.
    pub async fn is_current(&self) -> Result<bool> {
        self.journal.is_current().await
    }

    /// Whether the session can run more SQL. An SQL error (a syntax error, a
    /// constraint, a missing table) leaves it usable: the failed statement had
    /// no effect, and an open transaction continues. A storage error, a
    /// cancelled call, a failed commit or an exceeded limit ends it: discard it
    /// and open a new one, after settling outstanding requests.
    pub fn is_usable(&self) -> bool {
        !self.is_poisoned()
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned || self.abandoned.load(Ordering::SeqCst) || !self.journal.is_usable()
    }

    /// Run each statement of `sql`, applying the transaction rules after each:
    /// inside a [`Self::transaction`] body the transaction must stay open.
    /// With `params`, `sql` must be one statement.
    async fn run(&mut self, sql: &str, params: Option<Vec<Value>>) -> Result<Rows> {
        self.run_columns(sql, params).await.map(|(_, rows)| rows)
    }

    /// [`Self::run`], with the column names of the last statement.
    async fn run_columns(
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
                if connection.consume_stmt(remaining)?.is_some() {
                    self.poisoned = false;
                    return Err("parameters bind to a single statement".into());
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
            let limit = self.io.snapshot().options().max_result_bytes;
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

    /// Execute one statement and discard its collected result rows.
    /// This has the same private-write, error and cancellation behavior as [`Self::query`].
    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        self.query(sql).await.map(|_| ())
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

/// One namespace's journal: its paged files (the SQL database is the file
/// `db`, which [`Sql`] opens) and its records, changed in memory and made
/// durable together by [`Self::commit`], one put. A cheap handle; clones,
/// and the views of [`Self::scoped`], share the pending changes.
///
/// Acquire exclusive ownership of the namespace and settle the preceding
/// owner's requests before opening it; a reader ([`Self::open_reader`]) may
/// open beside the owner.
#[derive(Clone)]
pub struct Journal {
    shared: Arc<Shared>,
    /// Prefixed to every name: a view of part of the journal.
    prefix: Arc<str>,
}

/// What a journal's handles share.
struct Shared {
    io: Arc<KvIo>,
    options: StorageOptions,
    /// Records set and files written since the last commit, which the next
    /// one makes durable.
    records: Mutex<log::Records>,
    files: Mutex<snapshot::Writes>,
    /// The engine this owner's previous session left idle at the committed
    /// state, until [`Sql::open`] takes it.
    warm: Mutex<Option<WarmEngine>>,
    /// An [`Sql`] runs on the file `db`.
    sql: AtomicBool,
    /// A reader's ([`Journal::open_reader`]): it never commits.
    reader: AtomicBool,
    /// A commit failed or was cancelled: its outcome is unknown.
    poisoned: AtomicBool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // An engine no [`Sql`] took goes back, if nothing changed.
        let Some(warm) = self.warm.get_mut().unwrap().take() else {
            return;
        };
        if let Some(cache) = &self.options.cache
            && cache.committed() == Some(warm.version)
            && warm.io.version() == warm.version
            && warm.io.is_clean()
        {
            warm.io.idle();
            cache.park(warm);
        }
    }
}

/// The error of a journal or SQL session that a failure ended.
const ENDED: &str =
    "the session ended after a storage error, a cancelled call or a failed commit; open a new one";

impl Journal {
    /// Open the namespace with default [`StorageOptions`]: read its commit
    /// record (and the stored values it names), recovering what a crash
    /// left, before anything reads.
    pub async fn open(store: Arc<dyn KvStore>) -> Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }

    /// Open with explicit [`StorageOptions`]. Options never change the stored
    /// format: existing page tags determine decoding. With a [`PageCache`],
    /// an engine left idle by this owner's previous session waits for
    /// [`Sql::open`] when nothing changed since.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> Result<Self> {
        let snapshot = Snapshot::open_with_options(store, options.clone()).await?;
        let warm = match &options.cache {
            Some(cache) if snapshot.unchanged => cache
                .take_engine()
                .filter(|warm| warm.version == snapshot.version() && warm.io.is_clean()),
            _ => None,
        };
        let io = match &warm {
            Some(warm) => {
                warm.io.resume(snapshot).await?;
                warm.io.clone()
            }
            None => KvIo::from_snapshot(snapshot),
        };
        Ok(Self {
            shared: Arc::new(Shared {
                io,
                options,
                records: Mutex::default(),
                files: Mutex::default(),
                warm: Mutex::new(warm),
                sql: AtomicBool::new(false),
                reader: AtomicBool::new(false),
                poisoned: AtomicBool::new(false),
            }),
            prefix: Arc::from(""),
        })
    }

    /// Open a reader of a namespace that another owns, alongside it: a
    /// journal that writes nothing to the store (its changes stay private,
    /// and it refuses to commit) and keeps no cache. It reads the state as of
    /// open, unless the owner checkpoints meanwhile: [`Self::is_current`],
    /// once it has read, says whether it did.
    pub async fn open_reader(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<Self> {
        let options = StorageOptions {
            cache: None,
            ..options
        };
        let journal = Self::open_with_options(store, options).await?;
        journal.shared.reader.store(true, Ordering::SeqCst);
        Ok(journal)
    }

    /// The record named `name`: a whole value the commit record holds, which
    /// the next commit makes durable once set (see [`Record`]).
    pub fn record(&self, name: &str) -> Record {
        Record {
            name: format!("{}{name}", self.prefix).into(),
            shared: self.shared.clone(),
        }
    }

    /// The paged file named `name`: 4 KiB pages the next commit makes
    /// durable once written (see [`File`]). `db` is the SQL database's,
    /// which [`Sql`] opens.
    pub fn file(&self, name: &str) -> Result<File> {
        let name = format!("{}{name}", self.prefix);
        if name == snapshot::SQL_FILE {
            return Err(format!("{name:?} is the SQL database's file").into());
        }
        Ok(File {
            name: name.into(),
            shared: self.shared.clone(),
            base: None,
        })
    }

    /// The paged file named `name`, over `base` (see [`Base`]): an absent
    /// page, or a hole, reads as the base's page, and pages are compressed
    /// against it. A file made with a base opens only with that one again
    /// (by [`Base::id`]); its log since the last checkpoint decodes now.
    /// [`Self::file`] opens it without its base, to write holes and set its
    /// length only: emptying it for another base, say.
    pub fn file_with_base(&self, name: &str, base: Arc<dyn Base>) -> Result<File> {
        let mut file = self.file(name)?;
        self.shared.io.with_base(file.name(), base.clone())?;
        file.base = Some(base);
        Ok(file)
    }

    /// The part of the journal whose names start with `prefix`, named
    /// without it: an extension's, say (`scoped("fs/")` makes `manifest`
    /// the record `fs/manifest`).
    pub fn scoped(&self, prefix: &str) -> Journal {
        Journal {
            prefix: format!("{}{prefix}", self.prefix).into(),
            ..self.clone()
        }
    }

    /// The commit: every record set and file written since the last one,
    /// and the SQL database's pages [`Sql::flush`] moved into its file, in
    /// one put (and a checkpoint, when the log is long), then one sync.
    /// Nothing changed: nothing is written. `Ok` holds what it wrote; an
    /// error, or dropping the future, leaves the outcome unknown and ends
    /// the journal (and its [`Sql`]): open it again.
    pub async fn commit(&self) -> Result<SaveReport> {
        let shared = &self.shared;
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(ENDED.into());
        }
        if shared.reader.load(Ordering::SeqCst) {
            return Err("a reader writes nothing".into());
        }
        // Until the commit is acknowledged its outcome is unknown.
        shared.poisoned.store(true, Ordering::SeqCst);
        let (files, records) = self.take_pending();
        let report = shared.io.publish_with(files, records).await?;
        // The SQL engine's header, resident already unless the page size
        // grew. The commit is durable either way; a failure here only ends
        // the journal.
        if shared.io.prepare_engine().await.is_ok() {
            shared.poisoned.store(false, Ordering::SeqCst);
        }
        Ok(report)
    }

    /// Drop the records set and files written since the last commit.
    pub fn discard(&self) {
        self.take_pending();
    }

    /// The writes to files and records since the last commit, which the next
    /// one takes.
    fn take_pending(&self) -> (snapshot::Writes, log::Records) {
        let files = std::mem::take(&mut *self.shared.files.lock().unwrap());
        (
            files,
            std::mem::take(&mut *self.shared.records.lock().unwrap()),
        )
    }

    /// The namespace's version: the commits ever made, as this journal holds
    /// it (opened on, or committed itself).
    pub fn version(&self) -> u64 {
        self.shared.io.version()
    }

    /// Whether the store still holds the state this journal opened on (see
    /// [`Self::open_reader`]): if it does not, what was read may mix that
    /// state with a later checkpoint's pages, so read again in a new one. A
    /// journal's own commits change the state, so this is for one that has
    /// not committed.
    pub async fn is_current(&self) -> Result<bool> {
        Ok(self.shared.io.snapshot().is_current().await?)
    }

    /// Whether the journal can commit: a failed or cancelled commit ends it.
    pub fn is_usable(&self) -> bool {
        !self.shared.poisoned.load(Ordering::SeqCst)
    }
}

/// A named whole value the commit record holds, replaced whole: the host's
/// state beside its tables (a run's progress, an outbox), committed with them
/// in the same put. A cheap handle; clones share the session's pending
/// changes.
///
/// A value of up to [`StorageOptions::inline_log_bytes`] is in the record,
/// which every commit rewrites; a larger one is a stored value of its own,
/// written when it changes. Either way, opening the session reads it.
#[derive(Clone)]
pub struct Record {
    name: Arc<str>,
    shared: Arc<Shared>,
}

impl Record {
    /// The value as the session holds it: set since the last commit, or
    /// committed; empty if never set.
    pub fn get(&self) -> Bytes {
        if let Some(value) = self.shared.records.lock().unwrap().get(&*self.name) {
            return value.clone();
        }
        let snapshot = self.shared.io.snapshot();
        snapshot
            .records()
            .get(&*self.name)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the value with the next commit (see [`Journal::commit`]).
    pub fn set(&self, value: Bytes) {
        (self.shared.records.lock().unwrap()).insert(self.name.to_string(), value);
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A paged file beside the SQL database: 4 KiB pages, read and written at
/// random, made durable by the session's next commit with its SQL and its
/// records. An absent page, never written or past the end, reads as `None`
/// (zeros). A cheap handle; clones share the session's pending changes.
///
/// Its pages are kept as the SQL database's are: the commit record's log
/// holds the changed ones until a checkpoint writes them in place, and the
/// [`PageCache`] keeps them across sessions.
#[derive(Clone)]
pub struct File {
    name: Arc<str>,
    shared: Arc<Shared>,
    base: Option<Arc<dyn Base>>,
}

impl File {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bytes in the file: as written since the last commit, or committed.
    pub fn len(&self) -> u64 {
        match self.shared.files.lock().unwrap().get(&*self.name) {
            Some(written) => written.size,
            None => self.shared.io.snapshot().file_size(&self.name),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Page `page` as the session holds it; `None` for zeros (a hole of a
    /// file without a base, or past its end).
    pub async fn read(&self, page: u64) -> Result<Option<Page>> {
        if page * PAGE >= self.len() {
            return Ok(None);
        }
        let hole = || {
            let base = self.base.as_ref().and_then(|base| base.page(page));
            base.map(Page::copy_from_slice)
        };
        let written = self.shared.files.lock().unwrap().get(&*self.name).cloned();
        if let Some(image) = written.and_then(|written| written.pages.get(&page).cloned()) {
            return Ok(image.or_else(hole));
        }
        let snapshot = self.shared.io.snapshot();
        if page * PAGE >= snapshot.file_size(&self.name) {
            return Ok(hole());
        }
        Ok(snapshot.read(&self.name, page).await?)
    }

    /// Write `image` (4096 bytes) at `page`, or a hole with `None`, growing
    /// the file to include it; the next commit makes it durable.
    pub fn write(&self, page: u64, image: Option<Page>) -> Result<()> {
        if image
            .as_ref()
            .is_some_and(|image| image.len() != PAGE as usize)
        {
            return Err("a page is 4096 bytes".into());
        }
        let end = (page + 1) * PAGE;
        if end > log::MAX_DATABASE_BYTES {
            return Err("a page past the largest file".into());
        }
        let len = self.len();
        let mut pending = self.shared.files.lock().unwrap();
        let written = pending
            .entry(self.name.to_string())
            .or_insert_with(|| snapshot::Written {
                size: len,
                pages: Default::default(),
            });
        written.size = written.size.max(end);
        written.pages.insert(page, image);
        Ok(())
    }

    /// Make the file `len` bytes (a multiple of 4096): pages past it become
    /// holes, so regrowing it reads zeros.
    pub fn set_len(&self, len: u64) -> Result<()> {
        if !len.is_multiple_of(PAGE) || len > log::MAX_DATABASE_BYTES {
            return Err(format!("invalid file length {len}").into());
        }
        let old = self.len();
        let mut pending = self.shared.files.lock().unwrap();
        let written = pending
            .entry(self.name.to_string())
            .or_insert_with(|| snapshot::Written {
                size: old,
                pages: Default::default(),
            });
        written.pages.retain(|&page, _| page * PAGE < len);
        for page in len / PAGE..old.div_ceil(PAGE) {
            written.pages.insert(page, None);
        }
        written.size = len;
        Ok(())
    }
}

/// Bytes of a page.
const PAGE: u64 = io::BLOCK_SIZE as u64;

impl Drop for Sql {
    fn drop(&mut self) {
        // A clean read-only session leaves its engine for the next one.
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
                    return Err("result rows exceed StorageOptions::max_result_bytes".into());
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

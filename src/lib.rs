//! SQLite-compatible SQL over an asynchronous key/value store.
//!
//! The engine is Turso's Rust implementation (`turso_core`); this crate supplies
//! its file I/O and publishes immutable database snapshots. It requires only
//! [`kv::KvStore::get`], [`kv::KvStore::put`] and [`kv::KvStore::delete`], not a
//! multi-key transaction, compare-and-swap, listing operation or backend WAL API.
//!
//! # Two transaction boundaries
//!
//! SQL statements, SQL `COMMIT`, file sync and [`Session::checkpoint`] affect a
//! private in-memory view. They do **not** make changes durable in KV. Dropping a
//! session discards that view. [`Session::save`] checkpoints, stops the engine,
//! publishes the snapshot and consumes the session.
//!
//! | Layer | Representation | Survives dropping the session? |
//! |---|---|---|
//! | SQL working state | Dirty 4096-byte pages and private engine WAL | No |
//! | Committed pages | Immutable encoded page objects | Yes, after acknowledged save |
//! | Page map | Copy-on-write radix tree; root embedded in the head | Yes |
//! | Recovery intent | Exact old/new heads and created/retired object IDs | Until cleanup succeeds |
//!
//! A save with changes commits through one atomic, durable head replacement;
//! an unchanged save performs no mutations. If its
//! [`SaveReport::cleanup_error`] is set, the SQL changes **are committed**; reopening
//! retries reclamation. An error or cancellation can have an unknown commit outcome.
//! Settle outstanding backend requests and reopen before deciding whether to retry
//! application work. This crate does not provide application-level exactly-once IDs.
//!
//! # Why immutable pages and a map?
//!
//! SQLite addresses pages by stable page number, while a changed page receives a
//! new object ID. The radix map connects those two identities. A save rewrites only
//! changed pages and affected map paths; unchanged subtrees are reused. Publishing
//! one head switches the entire view, so partially uploaded objects cannot become
//! visible through the old snapshot.
//!
//! Overwriting stable page keys in place would destroy the old view before the
//! head commit and would need undo/redo logging or a multi-key transaction. A single
//! whole-database value would make atomic replacement simpler, but every small save
//! would rewrite the whole database. Persisting the engine WAL is another valid
//! design, with a different durability boundary and a durable log/checkpoint lifecycle.
//! Here, the engine WAL stays private and the durable intent stores IDs, not another
//! copy of page payloads.
//!
//! The costs are map lookups, path copying, temporary new objects and explicit
//! reclamation. The root lives in the head to save a dependent map request; a large
//! root increases head/intent bytes. There is one fixed map representation. This is
//! not MVCC: old readers must be gone before replaced objects are deleted.
//!
//! # Ownership and concurrency
//!
//! The host grants one owner exclusive use of a namespace, including settlement
//! of cancelled requests before reopening or handing ownership to another session.
//! There is no distributed lock or fence here. Every backend put/delete is awaited
//! before the next mutation starts, including recovery and cleanup. Independent
//! reads may overlap: open reads head and intent together, and the I/O pump batches
//! adjacent file reads without crossing write, truncate or sync barriers.
//!
//! One native worker thread drives the engine's blocking API; KV futures are polled
//! by the async caller. Queries run sequentially on a session, even though their
//! independent I/O can overlap. No executor-specific spawning is required.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use sqlite_kv::{Session, kv::KvStore};
//! # async fn example(store: Arc<dyn KvStore>) -> sqlite_kv::Result<()> {
//! let mut session = Session::open(store).await?;
//! session.execute("CREATE TABLE messages(body TEXT)").await?;
//! session.execute("INSERT INTO messages VALUES('hello')").await?;
//! let saved = session.save().await?;
//! if let Some(error) = saved.cleanup_error {
//!     // Committed: reopen to finish cleanup; do not replay the INSERT.
//!     eprintln!("cleanup pending: {error}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Limits
//!
//! The intent is capped at [`MAX_PENDING_BYTES`]. Dirty pages, private WAL, map
//! cache, write plans and query results do not have byte budgets. The current format
//! has structural validation but no checksum or corruption repair. Use a fresh,
//! dedicated namespace; no migration or format discovery is implemented. Simulation
//! and benchmark support require the opt-in `simulation` feature.
mod format;
mod map_codec;
pub mod page_codec;
pub use page_codec::PageCompression;
mod pending;
pub use pending::{MAX_PENDING_BYTES, SaveReport};
pub mod io;
pub mod kv;
#[cfg(feature = "simulation")]
pub mod simulation;
mod snapshot;
pub use snapshot::StorageOptions;

use crate::io::{DB_PATH, KvIo, MountedIo};
use crate::kv::KvStore;
use futures::{
    FutureExt, StreamExt,
    channel::{mpsc, oneshot},
};
use std::{
    sync::{Arc, mpsc as commands},
    thread,
};
use turso_core::storage::database::DatabaseFile;
use turso_core::{
    Connection, Database, IO, OpenFlags, OpenOptions, SqliteDialect, StepResult, TempStore, Value,
};

/// Adapter, engine and backend errors, safe to carry across the engine-worker boundary.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Rows = Vec<Vec<Value>>;

/// Commands sent from the async session owner to its single engine thread.
/// Replies carry SQL results or a lifecycle barrier; storage futures stay on the caller.
enum Command {
    Query(String, oneshot::Sender<Result<Rows>>),
    CanSave(oneshot::Sender<Result<()>>),
    Stop(oneshot::Sender<()>),
}

/// One private SQL working view over a recovered, committed snapshot.
///
/// Queries and checkpoints require exclusive access to this handle. A query error
/// or cancellation poisons it: discard it and reopen after settling backend work.
/// [`Self::save`] consumes the handle; dropping it never implicitly saves.
///
/// The host must exclude other sessions and old snapshot readers from the same
/// namespace. The worker-thread handle is detached on drop so cancellation does
/// not block the caller on an executing SQL statement.
pub struct Session {
    commands: commands::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    io: Arc<KvIo>,
    events: mpsc::UnboundedReceiver<()>,
    poisoned: bool,
}

impl Session {
    /// Local engine I/O only, useful for separating WAL traffic from backend writes.
    #[cfg(feature = "simulation")]
    pub fn io_stats(&self) -> crate::simulation::IoStats {
        self.io.stats()
    }

    /// Recover any pending publication and open a fresh engine with default compression.
    ///
    /// Acquire exclusive ownership and settle the preceding owner's requests first.
    /// Recovery may perform serial mutations before the session becomes usable.
    pub async fn open(store: Arc<dyn KvStore>) -> Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }

    /// Open with a page-encoding policy for future saves.
    /// Existing page tags determine decoding; changing policy requires no migration.
    /// Map representation and single-mutation ordering are fixed.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> Result<Self> {
        let io = KvIo::mount_with_options(store, options).await?;
        io.use_worker_completions();
        let events = io.take_events();
        let (commands, receiver) = commands::channel();
        let (ready, started) = oneshot::channel();
        let engine_io = io.clone();
        let worker = thread::Builder::new()
            .name("sqlite-kv-engine".into())
            .spawn(move || {
                engine_worker(engine_io, receiver, ready);
            })?;
        // Construct before awaiting: cancellation drops Session and wakes the worker.
        let mut session = Self {
            commands,
            worker: Some(worker),
            io,
            events,
            poisoned: true,
        };
        session.drive_until(started).await??;
        session.poisoned = false;
        Ok(session)
    }

    async fn drive_until<T>(&mut self, result: oneshot::Receiver<T>) -> Result<T> {
        let result = result.fuse();
        futures::pin_mut!(result);
        loop {
            futures::select! {
                value = result => return value.map_err(|_| "engine worker stopped unexpectedly".into()),
                event = self.events.next().fuse() => {
                    if event.is_none() { return Err("engine I/O channel closed".into()); }
                    self.io.drive().await?;
                }
            }
        }
    }

    /// Execute one SQL statement and collect all result rows in memory.
    ///
    /// There is no result byte limit or streaming cursor. SQL writes remain private
    /// until [`Self::save`]. Errors and cancellation poison this session.
    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        if self.poisoned {
            return Err("session is poisoned; discard and reopen".into());
        }
        self.poisoned = true; // cancellation/error requires discarding the session
        let (tx, rx) = oneshot::channel();
        self.commands.send(Command::Query(sql.to_owned(), tx))?;
        let rows = self.drive_until(rx).await??;
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
        let rows = self.query("PRAGMA wal_checkpoint(TRUNCATE)").await?;
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

    /// Checkpoints the private WAL, durably publishes only database pages, and consumes the session.
    /// Ok confirms durable publication. Inspect SaveReport::cleanup_error: Some
    /// means committed with pending cleanup, retried on open. Err/cancellation
    /// may have an unknown publication outcome: settle requests and reopen.
    pub async fn save(mut self) -> Result<SaveReport> {
        if self.poisoned {
            return Err("cannot save a poisoned session".into());
        }
        let (tx, rx) = oneshot::channel();
        self.commands.send(Command::CanSave(tx))?;
        self.drive_until(rx).await??;
        self.checkpoint().await?;
        let (tx, rx) = oneshot::channel();
        self.commands.send(Command::Stop(tx))?;
        self.drive_until(rx).await?;
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "engine worker panicked")?;
        }
        Ok(self.io.publish().await?)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Never block the host on an active query. Wake any engine I/O wait and
        // detach the thread; it exits after unwinding the current command.
        self.io.shutdown();
        let (tx, _) = oneshot::channel();
        let _ = self.commands.send(Command::Stop(tx));
    }
}

fn open_engine(io: Arc<KvIo>) -> Result<Arc<Connection>> {
    let mounted: Arc<dyn IO> = Arc::new(MountedIo(io));
    let file = mounted.open_file(DB_PATH, OpenFlags::Create, false)?;
    let options =
        OpenOptions::new(Arc::new(SqliteDialect)).storage(Arc::new(DatabaseFile::new(file)));
    // Bypass the global registry: reopening must construct a genuinely cold engine.
    let mut state = turso_core::OpenDbAsyncState::new();
    let db = loop {
        match Database::do_open_async(&mut state, mounted.clone(), DB_PATH, &options)? {
            turso_core::IOResult::Done(db) => break db,
            turso_core::IOResult::IO(completion) => completion.wait(&*mounted)?,
        }
    };
    let connection = db.connect()?;
    connection.wal_auto_actions_disable();
    connection.set_temp_store(TempStore::Memory);
    Ok(connection)
}

fn engine_worker(
    io: Arc<KvIo>,
    commands: commands::Receiver<Command>,
    ready: oneshot::Sender<Result<()>>,
) {
    let connection = match open_engine(io.clone()) {
        Ok(connection) => connection,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    while let Ok(command) = commands.recv() {
        match command {
            Command::Query(sql, response) => {
                let _ = response.send(query_engine(&connection, &io, &sql));
            }
            Command::CanSave(response) => {
                let result = if connection.get_auto_commit() {
                    Ok(())
                } else {
                    Err("cannot save an open SQL transaction".into())
                };
                let _ = response.send(result);
            }
            Command::Stop(response) => {
                // Drop releases locks without a hidden checkpoint or file I/O.
                drop(connection);
                let _ = response.send(());
                return;
            }
        }
    }
}

fn query_engine(connection: &Arc<Connection>, io: &Arc<KvIo>, sql: &str) -> Result<Rows> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = Vec::new();
    loop {
        match statement.step()? {
            StepResult::Row => rows.push(
                statement
                    .row()
                    .ok_or("missing row")?
                    .get_values()
                    .cloned()
                    .collect(),
            ),
            StepResult::Done => return Ok(rows),
            StepResult::IO => io.step()?,
            StepResult::Yield => thread::yield_now(),
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

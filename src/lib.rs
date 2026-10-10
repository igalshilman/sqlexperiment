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
//! The caller's task drives nonblocking engine open/connect state machines and
//! statement steps, awaits queued KV work, then resumes the engine. Completions run
//! on that task. No worker thread, command channel or background task is created.
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
use std::{future::poll_fn, sync::Arc, task::Poll};
use turso_core::storage::database::DatabaseFile;
use turso_core::{
    ConnectAsyncState, Connection, Database, IO, IOResult, OpenFlags, OpenOptions, SqliteDialect,
    StepResult, TempStore, Value,
};

/// Adapter, engine and backend errors returned to the host task.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Rows = Vec<Vec<Value>>;

/// One private SQL view, driven entirely by the caller's async task.
///
/// The host advances engine state machines, awaits queued KV I/O and resumes the
/// engine on the same task. No worker thread or background task is created.
/// Queries require exclusive access to this handle. An error or cancellation
/// poisons the view; discard it and reopen after settling backend requests.
/// [`Self::save`] consumes the handle; dropping it never implicitly saves.
pub struct Session {
    connection: Option<Arc<Connection>>,
    io: Arc<KvIo>,
    poisoned: bool,
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

/// Cooperatively return to any host executor, including with immediately-ready KV.
async fn yield_to_host() {
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
    /// The returned future is `Send`; all work is polled by the caller's executor.
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
        let mut session = Self {
            connection: None,
            io,
            poisoned: true,
        };
        session.io.prepare_engine().await?;
        session.connection = Some(open_engine(&session.io).await?);
        session.poisoned = false;
        Ok(session)
    }

    /// Execute one SQL statement and collect all result rows in memory.
    ///
    /// There is no result byte limit or streaming cursor. SQL writes remain private
    /// until [`Self::save`]. Errors and cancellation poison this session.
    /// Cached execution yields every 64 steps; one engine call cannot be preempted.
    /// An engine helper requiring synchronous nonresident I/O returns an error.
    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        if self.poisoned {
            return Err("session is poisoned; discard and reopen".into());
        }
        self.poisoned = true; // cancellation/error requires discarding the session
        let rows = query_engine(self.connection.as_ref().unwrap(), &self.io, sql).await?;
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
        if !self.connection.as_ref().unwrap().get_auto_commit() {
            return Err("cannot save an open SQL transaction".into());
        }
        self.checkpoint().await?;
        // Drop releases engine state without checkpointing or hidden file I/O.
        drop(self.connection.take());
        Ok(self.io.publish().await?)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.io.shutdown();
        drop(self.connection.take());
    }
}

/// Resume an engine completion only after the host has driven all submitted I/O.
async fn drive_completion(io: &KvIo, completion: turso_core::types::IOCompletions) -> Result<()> {
    if completion.is_explicit_yield() {
        yield_to_host().await;
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

async fn open_engine(io: &Arc<KvIo>) -> Result<Arc<Connection>> {
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

async fn query_engine(connection: &Arc<Connection>, io: &KvIo, sql: &str) -> Result<Rows> {
    let mut statement = connection.prepare(sql)?;
    let mut running = Running {
        io,
        complete: false,
    };
    let mut rows = Vec::new();
    let mut steps = 0;
    loop {
        // Bound work between host yields even when all pages are already cached.
        // A single engine step is not preemptible by this adapter.
        if steps == 64 {
            yield_to_host().await;
            steps = 0;
        }
        steps += 1;
        match statement.step()? {
            StepResult::Row => rows.push(
                statement
                    .row()
                    .ok_or("missing row")?
                    .get_values()
                    .cloned()
                    .collect(),
            ),
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
            StepResult::Yield => yield_to_host().await,
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

//! SQLite over asynchronous KV with atomic snapshot publication.
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
pub use snapshot::{MapLayout, StorageOptions};

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

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Rows = Vec<Vec<Value>>;

enum Command {
    Query(String, oneshot::Sender<Result<Rows>>),
    CanSave(oneshot::Sender<Result<()>>),
    Stop(oneshot::Sender<()>),
}

/// Single-owner session. Storage futures always run on the async caller.
/// A dedicated engine thread accommodates the SQLite engine's blocking internals.
/// Drop discards the overlay; there is no implicit save.
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

    pub async fn open(store: Arc<dyn KvStore>) -> Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }

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

    /// One SQL statement at a time. Results are collected in memory.
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

    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        self.query(sql).await.map(|_| ())
    }

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

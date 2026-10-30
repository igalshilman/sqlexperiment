//! The session the Node.js addon exposes, over a key/value store the host
//! implements in JavaScript.
//!
//! Unlike [`Sql`], a [`Session`] flushes the state itself: a call that
//! leaves no SQL transaction open moves the SQL into a mutation and flushes it
//! before it returns, so every SQL commit is durable when its call resolves.
//! The host's `transaction()` runs its body between [`Session::begin`] and
//! [`Session::commit`] or [`Session::rollback`]; meanwhile a statement that
//! ends the transaction ends the session, rather than commit the body halfway.
//!
//! Calls hold the session one at a time; the host orders them (and keeps
//! other calls out of a transaction). A session ends with [`Session::save`],
//! which leaves a clean engine to the next session of the namespace,
//! [`Session::close`], or a failure; later calls fail.
use crate::{Result, Rows, Sql, Value};
use bytes::Bytes;
use futures::lock::{Mutex, MutexGuard};
use state::{PageCache, PageCacheConfig, State, kv::KvStore};
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// The record a commit's effects are in.
const EFFECTS: &str = "effects";

/// One session of a namespace's database, shared by the host's calls.
pub struct Session {
    /// `None` once the session has ended.
    sql: Mutex<Option<Sql>>,
    /// A reader's session: it never flushes.
    reader: bool,
    /// Inside a `transaction()` body, from `begin` to its `commit` or
    /// `rollback`.
    in_body: AtomicBool,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: AtomicBool,
    /// The effects [`Self::commit`] set, for the call's flush to put in the
    /// same mutation as the SQL.
    effects: std::sync::Mutex<Option<Bytes>>,
}

impl Session {
    /// Open the database of `namespace` over `store`, kept warm in the
    /// process-wide page cache. A `reader` reads the namespace beside its
    /// owner (see [`state::Open::reader`]): from the packs the owner cached,
    /// writing nothing to the store or the cache, it reads the state it
    /// opened on throughout.
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
        Ok(Self {
            sql: Mutex::new(Some(Sql::open(&state).await?)),
            reader: state.is_reader(),
            in_body: AtomicBool::new(false),
            usable: AtomicBool::new(true),
            effects: std::sync::Mutex::new(None),
        })
    }

    /// Run SQL (one or more statements; `params` bind to a single one) and
    /// return the column names and rows of its last statement.
    pub async fn query(
        &self,
        sql: &str,
        params: Option<Vec<Value>>,
    ) -> Result<(Vec<String>, Rows)> {
        self.call(async |session| session.query_columns(sql, params).await)
            .await
    }

    /// `BEGIN` the transaction of a `transaction()` body: until its `commit`
    /// or `rollback`, a statement that ends it ends the session.
    pub async fn begin(&self) -> Result<()> {
        self.call(async |session| {
            session.execute("BEGIN").await?;
            self.in_body.store(true, Ordering::Release);
            Ok(())
        })
        .await
    }

    /// `COMMIT` the transaction of a `transaction()` body; with `effects`,
    /// they commit with it, as the record `effects`, in the same put, even if
    /// the transaction changed nothing. A `COMMIT` that fails sets no effects
    /// and leaves the transaction open, for `rollback`.
    pub async fn commit(&self, effects: Option<Bytes>) -> Result<()> {
        self.call(async |session| {
            self.in_body.store(false, Ordering::Release);
            // Nothing commits on a session a failure ended: the effects would
            // stay pending, and the call would resolve as if they committed.
            if !session.is_usable() {
                return Err(ENDED.into());
            }
            if let Some(effects) = &effects {
                if self.reader {
                    return Err("a reader commits no effects".into());
                }
                if effects.len() > state::MAX_RECORD_BYTES {
                    let (size, limit) = (effects.len(), state::MAX_RECORD_BYTES);
                    return Err(format!("effects of {size} bytes exceed {limit}").into());
                }
            }
            if session.in_transaction() {
                session.execute("COMMIT").await?;
            }
            *self.effects.lock().unwrap() = effects;
            Ok(())
        })
        .await
    }

    /// `ROLLBACK` the transaction of a `transaction()` body, if it is still
    /// open and no failure ended the session.
    pub async fn rollback(&self) -> Result<()> {
        self.call(async |session| {
            self.in_body.store(false, Ordering::Release);
            match session.is_usable() && session.in_transaction() {
                true => session.execute("ROLLBACK").await,
                false => Ok(()),
            }
        })
        .await
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error or a cancelled call ends it. A call that is running
    /// does not change the answer until it ends.
    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them; empty if none did.
    pub async fn effects(&self) -> Result<Bytes> {
        let sql = self.sql.lock().await;
        let session = sql.as_ref().ok_or(CLOSED)?;
        Ok(session.state().record(EFFECTS).get())
    }

    /// End the session, leaving its engine to the next one. It fails if a
    /// transaction was left open, whose changes are lost, or a failure ended
    /// the session.
    pub async fn save(&self) -> Result<()> {
        let session = self.sql.lock().await.take();
        self.usable.store(false, Ordering::Release);
        let session = session.ok_or(CLOSED)?;
        if !session.is_usable() {
            return Err(ENDED.into());
        }
        if session.in_transaction() {
            return Err("a transaction was left open: its changes are lost".into());
        }
        Ok(())
    }

    /// End the session, whatever state it is in.
    pub async fn close(&self) {
        self.sql.lock().await.take();
        self.usable.store(false, Ordering::Release);
    }

    /// Run `call` on the open session, then settle it. A statement that
    /// ended the transaction of a `transaction()` body ends the session.
    /// Otherwise a call that leaves no SQL transaction open flushes the
    /// state, even after an SQL error (the statements before it stand): what
    /// the SQL committed, with the effects set, in one mutation, durable before
    /// the call returns. A reader flushes nothing, and a session that a
    /// failure ended cannot. A call dropped before it returns ends the
    /// session.
    async fn call<T>(&self, call: impl AsyncFnOnce(&mut Sql) -> Result<T>) -> Result<T> {
        let mut running = Running {
            sql: self.sql.lock().await,
            usable: &self.usable,
            returned: false,
        };
        let session = running.sql.as_mut().ok_or(CLOSED)?;
        let result = call(session).await;
        let effects = self.effects.lock().unwrap().take();
        if self.in_body.load(Ordering::Acquire) && !session.in_transaction() {
            self.in_body.store(false, Ordering::Release);
            // `running` ends the session as it drops. A storage error that
            // ended the transaction too is the error to report.
            return match result {
                Err(error) if !session.is_usable() => Err(error),
                _ => Err("a statement ended the transaction of transaction(), whose body must not: the session has ended".into()),
            };
        }
        let flushed = match !self.reader && session.is_usable() && !session.in_transaction() {
            true => flush(session, effects).await,
            false => Ok(()),
        };
        self.usable.store(session.is_usable(), Ordering::Release);
        running.returned = true;
        flushed.and(result)
    }
}

/// A call holding the session: unless it returns, its state is unknown (a
/// cancelled statement, a commit whose outcome is unknown), and dropping
/// this ends the session.
struct Running<'a> {
    sql: MutexGuard<'a, Option<Sql>>,
    usable: &'a AtomicBool,
    returned: bool,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if !self.returned {
            self.usable.store(false, Ordering::Release);
            self.sql.take();
        }
    }
}

/// The error of a call after the session ended.
const CLOSED: &str = "the database is closed";
/// The error of a call that would commit after a failure ended the session.
const ENDED: &str = "a failure ended the session";

/// What the SQL committed, and `effects` if any, durable in one mutation.
async fn flush(session: &mut Sql, effects: Option<Bytes>) -> Result<()> {
    let state = session.state();
    let mut mutation = state.mutation();
    session.flush(&mut mutation).await?;
    if let Some(effects) = effects {
        mutation.set(&state.record(EFFECTS), effects)?;
    }
    mutation.flush().await?;
    Ok(())
}

/// The page cache shared by the process's sessions: 256 MiB of memory and
/// 1 GiB of disk, in a file under the temporary directory, or the file
/// `RESTATE_SQL_CACHE_FILE` names.
async fn page_cache() -> io::Result<PageCache> {
    static CACHE: Mutex<Option<PageCache>> = Mutex::new(None);
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

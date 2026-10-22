//! The session the language bindings expose (the Node.js addon, the Python
//! extension), over a key/value store the host implements in its language.
//!
//! Unlike [`Sql`], a [`Session`] commits the journal itself: a call that
//! leaves no SQL transaction open flushes and commits before it returns, so
//! every SQL commit is durable when its call resolves. The host's
//! `transaction()` runs its body between [`Session::begin`] and
//! [`Session::commit`] or [`Session::rollback`]; meanwhile a statement that
//! ends the transaction ends the session, rather than commit the body halfway.
//!
//! Calls hold the session one at a time; the host orders them (and keeps
//! other calls out of a transaction). A session ends with [`Session::save`],
//! which leaves a clean engine to the next session of the namespace,
//! [`Session::close`], or a failure; later calls fail.
use crate::{Result, Rows, Sql, Value};
use bytes::Bytes;
use futures::lock::Mutex;
use journal::{Journal, PageCache, PageCacheConfig, StorageOptions, kv::KvStore};
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
    /// A reader's session: it never commits.
    reader: bool,
    /// Inside a `transaction()` body, from `begin` to its `commit` or
    /// `rollback`.
    in_body: AtomicBool,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: AtomicBool,
}

impl Session {
    /// Open the database of `namespace` over `store`, with the process-wide
    /// page cache. A `reader` reads the namespace beside its owner (see
    /// [`Journal::open_reader`]): without the cache, writing nothing to the
    /// store, and [`Self::is_current`] says whether what it read is still the
    /// stored state.
    pub async fn open(store: Arc<dyn KvStore>, namespace: &str, reader: bool) -> Result<Self> {
        let journal = match reader {
            true => Journal::open_reader(store, StorageOptions::default()).await?,
            false => {
                let cache = Some(page_cache().await?.namespace(namespace));
                let options = StorageOptions {
                    cache,
                    ..StorageOptions::default()
                };
                Journal::open_with_options(store, options).await?
            }
        };
        Ok(Self {
            sql: Mutex::new(Some(Sql::open(&journal).await?)),
            reader,
            in_body: AtomicBool::new(false),
            usable: AtomicBool::new(true),
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
            if let Some(effects) = &effects {
                if self.reader {
                    return Err("a reader commits no effects".into());
                }
                if effects.len() > journal::MAX_RECORD_BYTES {
                    let (size, limit) = (effects.len(), journal::MAX_RECORD_BYTES);
                    return Err(format!("effects of {size} bytes exceed {limit}").into());
                }
            }
            if session.in_transaction() {
                session.execute("COMMIT").await?;
            }
            if let Some(effects) = effects {
                session.journal().record(EFFECTS).set(effects);
            }
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
    /// a storage error ends it. A call that is running does not change the
    /// answer until it ends.
    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them; empty if none did.
    pub async fn effects(&self) -> Result<Bytes> {
        let sql = self.sql.lock().await;
        let session = sql.as_ref().ok_or(CLOSED)?;
        Ok(session.journal().record(EFFECTS).get())
    }

    /// Whether the store still holds the state this reader opened on, so
    /// that what it read is: if not, read again in a new session (see
    /// [`Journal::is_current`]).
    pub async fn is_current(&self) -> Result<bool> {
        let sql = self.sql.lock().await;
        sql.as_ref().ok_or(CLOSED)?.is_current().await
    }

    /// End the session, leaving its engine to the next one. It fails if a
    /// transaction was left open, whose changes are lost, or a failure ended
    /// the session.
    pub async fn save(&self) -> Result<()> {
        let session = self.sql.lock().await.take();
        self.usable.store(false, Ordering::Release);
        let session = session.ok_or(CLOSED)?;
        if !session.is_usable() {
            return Err("a failure ended the session".into());
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
    /// Otherwise a call that leaves no SQL transaction open commits the
    /// journal, even after an SQL error (the statements before it stand): what
    /// the SQL committed, with the effects set, durable before the call
    /// returns. A reader commits nothing, and a session that a failure ended
    /// cannot.
    async fn call<T>(&self, call: impl AsyncFnOnce(&mut Sql) -> Result<T>) -> Result<T> {
        let mut sql = self.sql.lock().await;
        let session = sql.as_mut().ok_or(CLOSED)?;
        let result = call(session).await;
        if self.in_body.load(Ordering::Acquire) && !session.in_transaction() {
            self.in_body.store(false, Ordering::Release);
            self.usable.store(false, Ordering::Release);
            sql.take();
            return Err("a statement ended the transaction of transaction(), whose body must not: the session has ended".into());
        }
        let committed = match !self.reader && session.is_usable() && !session.in_transaction() {
            true => commit_journal(session).await,
            false => Ok(()),
        };
        self.usable.store(session.is_usable(), Ordering::Release);
        committed.and(result)
    }
}

/// The error of a call after the session ended.
const CLOSED: &str = "the database is closed";

async fn commit_journal(session: &mut Sql) -> Result<()> {
    session.flush().await?;
    session.journal().commit().await?;
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
    let named = std::env::var_os("RESTATE_SQL_CACHE_FILE").map(PathBuf::from);
    let file = named.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("restate-sql-{}.cache", std::process::id()))
    });
    let opened = PageCache::open(PageCacheConfig::new(&file, 256 << 20, 1 << 30)).await?;
    // The cache keeps the file open, and nothing else uses the temporary one:
    // unlinked now, it goes away with the process however it ends, rather
    // than staying behind (where unlinking an open file is refused, it stays).
    if named.is_none() {
        let _ = std::fs::remove_file(&file);
    }
    Ok(cache.insert(opened).clone())
}

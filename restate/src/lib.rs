//! SQL in a Restate virtual object, with no journal.
//!
//! A handler takes a [`SqlObjectContext`] instead of an `ObjectContext`. It is
//! the same context (it dereferences to it) plus [`SqlObjectContext::sql`],
//! which runs SQL against the object's database. A page cache shared by the
//! process's objects is built in. Creating tables is up to the handlers, such as
//! an `init` handler, or `CREATE TABLE IF NOT EXISTS` where they are used.
//!
//! ```rust,ignore
//! #[restate_sdk::object]
//! impl Account {
//!     #[handler]
//!     async fn init(&self, ctx: SqlObjectContext<'_>) -> HandlerResult<()> {
//!         ctx.sql(async |db| db.execute("CREATE TABLE IF NOT EXISTS balance(amount INTEGER)").await)
//!             .await
//!     }
//!
//!     #[handler]
//!     async fn deposit(&self, ctx: SqlObjectContext<'_>, amount: i64) -> HandlerResult<()> {
//!         ctx.sql(async |db| {
//!             db.execute_with("UPDATE balance SET amount = amount + ?", [Value::from_i64(amount)])
//!                 .await
//!         })
//!         .await // durable as a whole once this returns Ok
//!     }
//! }
//!
//! Endpoint::builder().bind(Account.into_service_definition().options(service_options()))
//! ```
//!
//! The database lives in the object's Restate state, through [`ObjectState`].
//! Durability and recovery come from this crate's storage protocol, not from the
//! journal: a service bound with [`service_options`] does not replay the journal
//! when an invocation is retried, so the handler runs again from the start
//! against the state as it is (at least once).
use bytes::Bytes;
use futures::future::BoxFuture;
use restate_sdk::endpoint::{ContextInternal, InputMetadata, ServiceOptions};
use restate_sdk::prelude::{
    ContextSideEffects, HandlerError, ObjectContext, RunFuture, TerminalError,
};
use sqlite_kv::kv::KvStore;
use sqlite_kv::{Durability, PageCache, PageCacheConfig, Session, StorageOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::OnceCell;

/// Options for a service whose handlers use [`SqlContext::with_sql`]: no
/// journal replay; lazy state, so an invocation does not start by loading every
/// page; and no journal kept after an invocation completes, since it holds every
/// page it read.
pub fn service_options() -> ServiceOptions {
    ServiceOptions::new()
        .journal_replay(false)
        .enable_lazy_state(true)
        .journal_retention(Duration::ZERO)
}

/// The context of an exclusive object handler, with the object's database.
///
/// It dereferences to the `ObjectContext`, so state, calls and `run` work as
/// usual.
pub struct SqlObjectContext<'ctx> {
    ctx: ObjectContext<'ctx>,
}

impl<'ctx> From<(&'ctx ContextInternal, InputMetadata)> for SqlObjectContext<'ctx> {
    fn from(value: (&'ctx ContextInternal, InputMetadata)) -> Self {
        Self {
            ctx: ObjectContext::from(value),
        }
    }
}

impl<'ctx> Deref for SqlObjectContext<'ctx> {
    type Target = ObjectContext<'ctx>;
    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl DerefMut for SqlObjectContext<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

/// The page cache shared by the process's objects: 256 MiB of memory and 1 GiB
/// of disk, in a file under the temporary directory, or the file
/// `SQLITE_KV_CACHE_FILE` names.
static CACHE: OnceCell<PageCache> = OnceCell::const_new();
static WRAP: OnceLock<Wrap> = OnceLock::new();
type Wrap = fn(Arc<ObjectState>) -> Arc<dyn KvStore>;

/// Wrap each object's store, such as for fault injection in tests.
#[doc(hidden)]
pub fn set_store_wrapper(wrap: Wrap) -> bool {
    WRAP.set(wrap).is_ok()
}

async fn page_cache() -> Result<&'static PageCache, HandlerError> {
    CACHE
        .get_or_try_init(async || {
            let file = std::env::var_os("SQLITE_KV_CACHE_FILE").map_or_else(
                || std::env::temp_dir().join(format!("sqlite-kv-{}.cache", std::process::id())),
                PathBuf::from,
            );
            PageCache::open(PageCacheConfig::new(file, 256 << 20, 1 << 30)).await
        })
        .await
        .map_err(|e| HandlerError::from(e.to_string()))
}

impl SqlObjectContext<'_> {
    /// Run `body` against the object's database as one SQL transaction: it
    /// commits, durably, when `body` returns `Ok`, and rolls back on `Err`.
    ///
    /// An SQL error, or the body's own, is terminal, since a retry would repeat
    /// it. A storage error, which ends the session, is retryable: the handler
    /// runs again from the start.
    pub async fn sql<T>(
        &self,
        body: impl AsyncFnOnce(&mut Session) -> sqlite_kv::Result<T>,
    ) -> Result<T, HandlerError> {
        let internal = self.internal();
        if internal.journal_replay() {
            return Err(TerminalError::new(
                "bind the service with sqlite_kv_restate::service_options(): a replayed \
                 journal would not match the reads of a later attempt",
            )
            .into());
        }
        let namespace = format!("{}/{}", internal.service_name(), self.key());
        let cache = page_cache().await?.namespace(&namespace);
        let state = Arc::new(ObjectState::new(&self.ctx));
        let store = match WRAP.get() {
            Some(wrap) => wrap(state),
            None => state,
        };
        let options = StorageOptions {
            cache: Some(cache),
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let mut db = Session::open_with_options(store, options).await?;
        let result = db.transaction(body).await.map_err(|e| classify(&db, e));
        // Saving confirms the session's writes (an empty `ctx.run`, when it
        // wrote), so that the page cache may trust them. A session that does not
        // get there makes the next one in this process start cold.
        if db.is_usable() {
            db.save().await?;
        }
        result
    }
}

/// The handler error for `error`, which `db` returned: terminal while `db` is
/// usable, since then it was an SQL error (or the body's own) that a retry would
/// repeat; retryable once a storage error, cancellation or failed commit ended
/// the session, since a new attempt reopens it.
fn classify(db: &Session, error: Box<dyn std::error::Error + Send + Sync>) -> HandlerError {
    if db.is_usable() {
        TerminalError::new(error.to_string()).into()
    } else {
        error.into()
    }
}

/// Wait until every state write so far is stored: an empty `ctx.run`, whose
/// result the runtime acknowledges after the commands before it. A handler needs
/// it only before an effect outside Restate that must not happen for lost
/// writes; calls, the response and later writes are ordered after them anyway.
pub async fn sync(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    ctx.run(async || Ok(())).name("sync").await?;
    Ok(())
}

/// The state of one virtual object key, as a [`KvStore`].
///
/// A get reads the state: from the runtime, or locally if this attempt wrote
/// the key. A put or delete is a state write, which returns at once: the
/// runtime stores an invocation's commands in order, and after a failed attempt
/// keeps a prefix of them. So a write that is lost takes every later command
/// with it, including later writes, calls and the response: the store is
/// ordered ([`KvStore::ordered`]), and [`KvStore::sync`] is an empty `ctx.run`.
pub struct ObjectState {
    ctx: ContextInternal,
}

impl ObjectState {
    pub fn new(ctx: &ObjectContext<'_>) -> Self {
        Self {
            ctx: ctx.internal().clone(),
        }
    }
}

impl KvStore for ObjectState {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        let read = self.ctx.get::<Bytes>(key);
        Box::pin(async move { read.await.map_err(|e| io::Error::other(e.to_string())) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        self.ctx.set(key, value);
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.ctx.clear(key);
        Box::pin(std::future::ready(Ok(())))
    }

    /// A value the runtime returns was stored in its log first.
    fn durable_when_visible(&self) -> bool {
        true
    }

    fn ordered(&self) -> bool {
        true
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async {
            let run = self.ctx.run(async || Ok(())).name("sync").await;
            run.map_err(|e| io::Error::other(e.to_string()))
        })
    }
}

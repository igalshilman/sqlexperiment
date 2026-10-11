//! SQL in a Restate virtual object, with no journal.
//!
//! An exclusive handler runs SQL against its object's database with
//! [`SqlContext::with_sql`]; opening, the page cache and the schema are set up
//! once per process with [`Sql`].
//!
//! ```rust,ignore
//! #[restate_sdk::object]
//! impl Ledger {
//!     #[handler]
//!     async fn deposit(&self, ctx: ObjectContext<'_>, amount: i64) -> HandlerResult<()> {
//!         ctx.with_sql(async |db| {
//!             db.execute_with("UPDATE balance SET amount = amount + ?", [Value::from_i64(amount)])
//!                 .await
//!         })
//!         .await // durable as a whole once this returns Ok
//!     }
//! }
//!
//! Sql::new()
//!     .cache(PageCache::open(PageCacheConfig::new(dir, 256 << 20, 8 << 30)).await?)
//!     .schema("Ledger", "CREATE TABLE balance(amount INTEGER); INSERT INTO balance VALUES(0)")
//!     .install();
//! Endpoint::builder().bind(Ledger.into_service_definition().options(service_options()))
//! ```
//!
//! The database lives in the object's Restate state, through [`ObjectState`].
//! Durability and recovery come from this crate's storage protocol, not from the
//! journal: a service bound with [`service_options`] does not replay the journal
//! when an invocation is retried, so the handler runs again from the start
//! against the state as it is (at least once).
use bytes::Bytes;
use futures::future::BoxFuture;
use restate_sdk::endpoint::{ContextInternal, ServiceOptions};
use restate_sdk::prelude::{
    ContextSideEffects, HandlerError, ObjectContext, RunFuture, TerminalError,
};
use sqlite_kv::kv::KvStore;
use sqlite_kv::{Durability, PageCache, Session, StorageOptions};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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

/// The process's SQL setup: a page cache shared by every object, and each
/// service's schema. Without [`Sql::install`], objects run with no cache and
/// no schema.
#[derive(Default)]
pub struct Sql {
    cache: Option<PageCache>,
    schemas: HashMap<String, String>,
    wrap: Option<Wrap>,
    /// What this process knows of each namespace it opened.
    namespaces: Mutex<HashMap<String, Known>>,
}

type Wrap = fn(Arc<ObjectState>) -> Arc<dyn KvStore>;

/// What a process knows of one object's database.
#[derive(Clone, Copy, Default)]
struct Known {
    /// The last session's writes are stored, so the page cache's state for the
    /// namespace describes storage.
    confirmed: bool,
    /// The schema exists, as of a confirmed session.
    schema: bool,
}

static SQL: OnceLock<Sql> = OnceLock::new();

impl Sql {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep pages and a warm engine of each object across its invocations.
    pub fn cache(mut self, cache: PageCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// SQL creating the tables of `service`'s objects (one or more
    /// statements), run in the first session of each object's database.
    pub fn schema(mut self, service: &str, sql: impl Into<String>) -> Self {
        self.schemas.insert(service.to_owned(), sql.into());
        self
    }

    /// Wrap each object's store, such as for fault injection in tests.
    #[doc(hidden)]
    pub fn wrap_store(mut self, wrap: Wrap) -> Self {
        self.wrap = Some(wrap);
        self
    }

    /// Use this setup for the process. Panics if one is already in use.
    pub fn install(self) {
        if SQL.set(self).is_err() {
            panic!("sqlite_kv_restate::Sql is already installed");
        }
    }
}

/// SQL on the context of an exclusive object handler.
pub trait SqlContext {
    /// Run `body` against the object's database as one SQL transaction: it
    /// commits, durably, when `body` returns `Ok`, and rolls back on `Err`.
    ///
    /// An SQL error, or the body's own, is terminal, since a retry would repeat
    /// it. A storage error, which ends the session, is retryable: the handler
    /// runs again from the start.
    fn with_sql<T>(
        &self,
        body: impl AsyncFnOnce(&mut Session) -> sqlite_kv::Result<T>,
    ) -> impl Future<Output = Result<T, HandlerError>>;
}

impl SqlContext for ObjectContext<'_> {
    async fn with_sql<T>(
        &self,
        body: impl AsyncFnOnce(&mut Session) -> sqlite_kv::Result<T>,
    ) -> Result<T, HandlerError> {
        let sql = SQL.get_or_init(Sql::default);
        let internal = self.internal();
        if internal.journal_replay() {
            return Err(TerminalError::new(
                "bind the service with sqlite_kv_restate::service_options(): a replayed \
                 journal would not match the reads of a later attempt",
            )
            .into());
        }
        let namespace = format!("{}/{}", internal.service_name(), self.key());
        let cache = sql.cache.as_ref().map(|cache| cache.namespace(&namespace));

        // A session whose writes were not confirmed may have lost them, and the
        // cache would describe writes that do not exist: start over.
        let known = {
            let mut namespaces = sql.namespaces.lock().unwrap();
            let known = namespaces.entry(namespace.clone()).or_default();
            if !known.confirmed {
                if let Some(cache) = &cache {
                    cache.reset();
                }
                known.schema = false;
            }
            known.confirmed = false;
            *known
        };

        let state = Arc::new(ObjectState::new(self));
        let store = match sql.wrap {
            Some(wrap) => wrap(state.clone()),
            None => state.clone(),
        };
        let options = StorageOptions {
            cache: cache.clone(),
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let mut db = Session::open_with_options(store, options).await?;
        if !known.schema
            && let Some(schema) = sql.schemas.get(internal.service_name())
        {
            create(&mut db, schema)
                .await
                .map_err(|e| classify(&db, e))?;
        }
        let result = db.transaction(body).await.map_err(|e| classify(&db, e));
        drop(db);

        // Only a cache trusts this session's writes beyond it, and only once
        // they are stored: wait for them then.
        if cache.is_some() && state.wrote() {
            sync(self).await?;
        }
        let mut namespaces = sql.namespaces.lock().unwrap();
        namespaces.insert(
            namespace,
            Known {
                confirmed: true,
                schema: true,
            },
        );
        result
    }
}

/// Run `schema` if the database has no tables yet.
async fn create(db: &mut Session, schema: &str) -> sqlite_kv::Result<()> {
    let tables = db
        .query("SELECT count(*) FROM sqlite_schema WHERE type = 'table'")
        .await?;
    if tables[0][0].as_int() == Some(0) {
        db.transaction(async |db| db.execute(schema).await).await?;
    }
    Ok(())
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
/// with it, including later writes, calls and the response. What outlives the
/// attempt in this process, the page cache, waits for [`sync`] instead.
pub struct ObjectState {
    ctx: ContextInternal,
    wrote: AtomicBool,
}

impl ObjectState {
    pub fn new(ctx: &ObjectContext<'_>) -> Self {
        Self {
            ctx: ctx.internal().clone(),
            wrote: AtomicBool::new(false),
        }
    }

    /// Whether any write was issued.
    pub fn wrote(&self) -> bool {
        self.wrote.load(Ordering::Relaxed)
    }
}

impl KvStore for ObjectState {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        let read = self.ctx.get::<Bytes>(key);
        Box::pin(async move { read.await.map_err(|e| io::Error::other(e.to_string())) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        self.wrote.store(true, Ordering::Relaxed);
        self.ctx.set(key, value);
        Box::pin(std::future::ready(Ok(())))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.wrote.store(true, Ordering::Relaxed);
        self.ctx.clear(key);
        Box::pin(std::future::ready(Ok(())))
    }

    /// A value the runtime returns was stored in its log first.
    fn durable_when_visible(&self) -> bool {
        true
    }
}

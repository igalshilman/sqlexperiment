//! SQL in a Restate virtual object, with no journal.
//!
//! The object's database lives in its Restate state, through [`ObjectState`]: a
//! [`KvStore`] over the state of one virtual object key, read lazily. Durability
//! and recovery come from this crate's storage protocol, not from the journal: a
//! service bound with [`service_options`] does not replay the journal when an
//! invocation is retried, so the handler runs again from the start against the
//! state as it is (at least once).
//!
//! ```rust,ignore
//! #[restate_sdk::object]
//! impl Ledger {
//!     #[handler]
//!     async fn deposit(&self, ctx: ObjectContext<'_>, amount: i64) -> HandlerResult<()> {
//!         let mut db = sqlite_kv_restate::open(&ctx, Some(&self.cache)).await?;
//!         db.execute_with("UPDATE balance SET amount = amount + ?", [Value::Integer(amount)])
//!             .await?; // durable once this returns
//!         Ok(())
//!     }
//! }
//!
//! Endpoint::builder().bind(Ledger::new().into_service_definition().options(service_options()))
//! ```
use bytes::Bytes;
use futures::future::BoxFuture;
use restate_sdk::endpoint::{ContextInternal, ServiceOptions};
use restate_sdk::prelude::{
    ContextSideEffects, HandlerError, ObjectContext, RunFuture, TerminalError,
};
use sqlite_kv::kv::KvStore;
use sqlite_kv::{Durability, PageCache, Session, StorageOptions};
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Options for a service whose handlers use [`open`]: no journal replay; lazy
/// state, so an invocation does not start by loading every page; and no journal
/// kept after an invocation completes, since it holds every page it read.
pub fn service_options() -> ServiceOptions {
    ServiceOptions::new()
        .journal_replay(false)
        .enable_lazy_state(true)
        .journal_retention(Duration::ZERO)
}

/// Open the object's database for this invocation. Every SQL commit is durable
/// when it returns ([`Durability::Commit`]); dropping the session ends it.
/// `cache` keeps pages and a warm engine across invocations in this process.
pub async fn open(
    ctx: &ObjectContext<'_>,
    cache: Option<&PageCache>,
) -> Result<Session, HandlerError> {
    let options = storage_options(ctx, cache)?;
    Ok(Session::open_with_options(Arc::new(ObjectState::new(ctx)), options).await?)
}

/// The options [`open`] uses, for a host that wraps [`ObjectState`] in its own
/// store. Fails unless the service was bound with [`service_options`].
pub fn storage_options(
    ctx: &ObjectContext<'_>,
    cache: Option<&PageCache>,
) -> Result<StorageOptions, HandlerError> {
    let internal = ctx.internal();
    if internal.journal_replay() {
        return Err(TerminalError::new(
            "bind the service with sqlite_kv_restate::service_options(): a replayed journal \
             would not match the reads of a later attempt",
        )
        .into());
    }
    let namespace = format!("{}/{}", internal.service_name(), ctx.key());
    Ok(StorageOptions {
        cache: cache.map(|cache| cache.namespace(&namespace)),
        durability: Durability::Commit,
        ..StorageOptions::default()
    })
}

/// The handler error for `error`, which `db` returned: terminal while `db` is
/// usable, since then it was an SQL error (or the body's own) that a retry would
/// repeat; retryable once a storage error, cancellation or failed commit ended
/// the session, since a new attempt reopens it.
pub fn classify(db: &Session, error: Box<dyn std::error::Error + Send + Sync>) -> HandlerError {
    if db.is_usable() {
        TerminalError::new(error.to_string()).into()
    } else {
        error.into()
    }
}

/// The state of one virtual object key, as a [`KvStore`].
///
/// A get reads the state: from the runtime, or locally if this attempt wrote
/// the key. A put or delete is a state write, which returns at once: the
/// runtime stores an invocation's commands in order, and after a crash keeps a
/// prefix of them. So a write that is lost takes every later command with it,
/// including later writes, calls and the response; nothing that depends on it
/// can outlive it. Before an effect Restate does not order, such as a request
/// to another system, call [`sync`].
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
}

/// Wait until every state write so far is stored: an empty `ctx.run`, whose
/// result the runtime acknowledges after the commands before it. Needed only
/// before an effect outside Restate that must not happen for lost writes.
pub async fn sync(ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
    ctx.run(async || Ok(())).name("sync").await?;
    Ok(())
}

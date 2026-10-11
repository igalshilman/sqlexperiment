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
use futures::future::{BoxFuture, FutureExt, Shared};
use restate_sdk::endpoint::{ContextInternal, ServiceOptions};
use restate_sdk::prelude::{HandlerError, ObjectContext, TerminalError};
use sqlite_kv::kv::KvStore;
use sqlite_kv::{Durability, PageCache, Session, StorageOptions};
use std::future::poll_fn;
use std::io;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

/// A state key nobody writes: reading it from the runtime is the durability
/// barrier for every state write before it.
const BARRIER_KEY: &str = "sqlite-kv/barrier";

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
/// A get reads the state (from the runtime, unless this attempt wrote the key).
/// A put or delete is a state write, which the runtime stores and applies in
/// order with the invocation's other commands; it is durable once a later read
/// from the runtime is answered. So a write awaits a read of [`BARRIER_KEY`],
/// shared by the writes issued together.
pub struct ObjectState {
    ctx: ContextInternal,
    barrier: Mutex<Barrier>,
}

type Pending = Shared<BoxFuture<'static, Result<(), String>>>;

#[derive(Default)]
struct Barrier {
    /// Writes issued so far.
    issued: u64,
    /// Writes the pending barrier was issued after.
    covered: u64,
    pending: Option<Pending>,
}

impl ObjectState {
    pub fn new(ctx: &ObjectContext<'_>) -> Self {
        Self {
            ctx: ctx.internal().clone(),
            barrier: Mutex::default(),
        }
    }

    /// Wait until the write just issued is durable.
    async fn written(&self) -> io::Result<()> {
        let ticket = {
            let mut barrier = self.barrier.lock().unwrap();
            barrier.issued += 1;
            barrier.issued
        };
        // Let the writes polled with this one be issued first, to share a barrier.
        let mut yielded = false;
        poll_fn(|cx| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        let pending = {
            let mut barrier = self.barrier.lock().unwrap();
            match &barrier.pending {
                Some(pending) if barrier.covered >= ticket => pending.clone(),
                _ => {
                    // Issued when first polled, which is after every write it covers.
                    let ctx = self.ctx.clone();
                    let pending = async move {
                        let read = ctx.get_lazy::<Bytes>(BARRIER_KEY).await;
                        read.map(drop).map_err(|e| e.to_string())
                    }
                    .boxed()
                    .shared();
                    barrier.covered = barrier.issued;
                    barrier.pending = Some(pending.clone());
                    pending
                }
            }
        };
        pending.await.map_err(io::Error::other)
    }
}

impl KvStore for ObjectState {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        let read = self.ctx.get::<Bytes>(key);
        Box::pin(async move { read.await.map_err(|e| io::Error::other(e.to_string())) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.ctx.set(key, value);
            self.written().await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.ctx.clear(key);
            self.written().await
        })
    }

    /// A value the runtime returns was stored in its log first.
    fn durable_when_visible(&self) -> bool {
        true
    }
}

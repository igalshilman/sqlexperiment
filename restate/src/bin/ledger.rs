//! A ledger virtual object: accounts and transfers in SQL, with no journal.
//!
//! Each transfer carries an id and is recorded in the same SQL transaction as
//! the balances it moves, so a retried invocation (at least once) applies it
//! once. `LEDGER_ABORT_PER_MILLE` aborts the process at random state requests,
//! for crash tests; `LEDGER_KEEP_JOURNAL` keeps completed journals for an hour;
//! `PORT` sets the listening port (default 9080).
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlite_kv::kv::KvStore;
use sqlite_kv::{PageCache, PageCacheConfig, Session, Value};
use sqlite_kv_restate::{ObjectState, classify, service_options, storage_options};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct Ledger {
    cache: Option<PageCache>,
    abort_per_mille: u64,
}

#[derive(Serialize, Deserialize)]
struct Transfer {
    id: String,
    from: i64,
    to: i64,
    amount: i64,
}

#[derive(Serialize, Deserialize)]
struct Audit {
    accounts: i64,
    total: i64,
    transfers: i64,
    /// Accounts whose balance differs from their opening balance plus the
    /// recorded transfers.
    inconsistent: i64,
}

impl Ledger {
    async fn open(&self, ctx: &ObjectContext<'_>) -> Result<Session, HandlerError> {
        let options = storage_options(ctx, self.cache.as_ref())?;
        let state = ObjectState::new(ctx);
        let store: Arc<dyn KvStore> = if self.abort_per_mille > 0 {
            Arc::new(Abort {
                inner: state,
                per_mille: self.abort_per_mille,
            })
        } else {
            Arc::new(state)
        };
        Ok(Session::open_with_options(store, options).await?)
    }
}

#[restate_sdk::object]
impl Ledger {
    /// Open `count` accounts with `balance` each, once.
    #[handler]
    async fn init(&self, ctx: ObjectContext<'_>, request: Json<(i64, i64)>) -> HandlerResult<()> {
        let (count, balance) = request.into_inner();
        let mut db = self.open(&ctx).await?;
        db.transaction(async |db| {
            db.execute(
                "CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, opening INTEGER, balance INTEGER);
                 CREATE TABLE IF NOT EXISTS transfers(id TEXT PRIMARY KEY, src INTEGER, dst INTEGER, amount INTEGER);",
            )
            .await?;
            for id in 0..count {
                db.execute_with(
                    "INSERT OR IGNORE INTO accounts VALUES(?, ?, ?)",
                    [Value::from_i64(id), Value::from_i64(balance), Value::from_i64(balance)],
                )
                .await?;
            }
            Ok(())
        })
        .await
        .map_err(|e| classify(&db, e))
    }

    /// Move money between accounts; false if this transfer id was applied before.
    #[handler]
    async fn transfer(&self, ctx: ObjectContext<'_>, t: Json<Transfer>) -> HandlerResult<bool> {
        let t = t.into_inner();
        let mut db = self.open(&ctx).await?;
        let applied = db
            .transaction(async |db| {
                let seen = db
                    .query_with(
                        "SELECT 1 FROM transfers WHERE id = ?",
                        [Value::build_text(t.id.clone())],
                    )
                    .await?;
                if !seen.is_empty() {
                    return Ok(false);
                }
                db.execute_with(
                    "INSERT INTO transfers VALUES(?, ?, ?, ?)",
                    [
                        Value::build_text(t.id.clone()),
                        Value::from_i64(t.from),
                        Value::from_i64(t.to),
                        Value::from_i64(t.amount),
                    ],
                )
                .await?;
                db.execute_with(
                    "UPDATE accounts SET balance = balance - ?1 WHERE id = ?2",
                    [Value::from_i64(t.amount), Value::from_i64(t.from)],
                )
                .await?;
                db.execute_with(
                    "UPDATE accounts SET balance = balance + ?1 WHERE id = ?2",
                    [Value::from_i64(t.amount), Value::from_i64(t.to)],
                )
                .await?;
                Ok(true)
            })
            .await
            .map_err(|e| classify(&db, e))?;
        Ok(applied)
    }

    /// Totals, and accounts that disagree with the recorded transfers.
    #[handler]
    async fn audit(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<Audit>> {
        let mut db = self.open(&ctx).await?;
        let integrity = db
            .query("PRAGMA integrity_check")
            .await
            .map_err(|e| classify(&db, e))?;
        if integrity.len() != 1 || integrity[0][0].to_string() != "ok" {
            return Err(TerminalError::new("integrity check failed").into());
        }
        let rows = db
            .query(
                "SELECT (SELECT count(*) FROM accounts), (SELECT sum(balance) FROM accounts),
                        (SELECT count(*) FROM transfers),
                        (SELECT count(*) FROM accounts a WHERE a.balance != a.opening
                            - (SELECT coalesce(sum(amount), 0) FROM transfers WHERE src = a.id)
                            + (SELECT coalesce(sum(amount), 0) FROM transfers WHERE dst = a.id))",
            )
            .await
            .map_err(|e| classify(&db, e))?;
        let int = |i: usize| rows[0][i].as_int().unwrap_or(0);
        // Nothing here needs it, since the audit writes nothing: it shows where a
        // handler would wait for its writes before acting outside Restate.
        sqlite_kv_restate::sync(&ctx).await?;
        Ok(Json(Audit {
            accounts: int(0),
            total: int(1),
            transfers: int(2),
            inconsistent: int(3),
        }))
    }
}

/// Aborts the process before or after a random state request.
struct Abort {
    inner: ObjectState,
    per_mille: u64,
}

impl Abort {
    fn maybe(&self) {
        static STATE: AtomicU64 = AtomicU64::new(0);
        let mut x = STATE.load(Ordering::Relaxed);
        if x == 0 {
            x = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        STATE.store(x, Ordering::Relaxed);
        if x % 1000 < self.per_mille {
            eprintln!("ledger: aborting at a state request");
            std::process::abort();
        }
    }
}

impl KvStore for Abort {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> futures::future::BoxFuture<'a, io::Result<Option<bytes::Bytes>>> {
        Box::pin(async move {
            self.maybe();
            self.inner.get(key).await
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: bytes::Bytes,
    ) -> futures::future::BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.maybe();
            let result = self.inner.put(key, value).await;
            self.maybe();
            result
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> futures::future::BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.maybe();
            let result = self.inner.delete(key).await;
            self.maybe();
            result
        })
    }

    fn durable_when_visible(&self) -> bool {
        self.inner.durable_when_visible()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let port: u16 = std::env::var("PORT").map_or(9080, |p| p.parse().expect("PORT"));
    let abort_per_mille =
        std::env::var("LEDGER_ABORT_PER_MILLE").map_or(0, |p| p.parse().expect("per mille"));
    let cache = match std::env::var("LEDGER_CACHE_DIR") {
        Ok(dir) => Some(
            PageCache::open(PageCacheConfig::new(dir, 64 << 20, 256 << 20))
                .await
                .expect("page cache"),
        ),
        Err(_) => None,
    };
    let ledger = Ledger {
        cache,
        abort_per_mille,
    };
    let mut options = service_options()
        .retry_policy_initial_interval(Duration::from_millis(50))
        .retry_policy_max_interval(Duration::from_millis(500));
    if std::env::var_os("LEDGER_KEEP_JOURNAL").is_some() {
        // To inspect what invocations journaled.
        options = options.journal_retention(Duration::from_secs(3600));
    }
    let endpoint = Endpoint::builder()
        .bind(ledger.into_service_definition().options(options))
        .build();
    // Rebind at once after a crash, past connections in TIME_WAIT.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind(([127, 0, 0, 1], port).into()).unwrap();
    HttpServer::new(endpoint)
        .serve(socket.listen(1024).unwrap())
        .await;
}

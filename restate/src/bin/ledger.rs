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
use sqlite_kv::{PageCache, PageCacheConfig, Value};
use sqlite_kv_restate::{ObjectState, Sql, SqlContext, service_options};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct Ledger;

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

const SCHEMA: &str = "
    CREATE TABLE accounts(id INTEGER PRIMARY KEY, opening INTEGER, balance INTEGER);
    CREATE TABLE transfers(id TEXT PRIMARY KEY, src INTEGER, dst INTEGER, amount INTEGER);
";

#[restate_sdk::object]
impl Ledger {
    /// Open `count` accounts with `balance` each, once.
    #[handler]
    async fn init(&self, ctx: ObjectContext<'_>, request: Json<(i64, i64)>) -> HandlerResult<()> {
        let (count, balance) = request.into_inner();
        ctx.with_sql(async |db| {
            for id in 0..count {
                db.execute_with(
                    "INSERT OR IGNORE INTO accounts VALUES(?, ?, ?)",
                    [
                        Value::from_i64(id),
                        Value::from_i64(balance),
                        Value::from_i64(balance),
                    ],
                )
                .await?;
            }
            Ok(())
        })
        .await
    }

    /// Move money between accounts; false if this transfer id was applied before.
    #[handler]
    async fn transfer(&self, ctx: ObjectContext<'_>, t: Json<Transfer>) -> HandlerResult<bool> {
        let t = t.into_inner();
        ctx.with_sql(async |db| {
            let id = Value::build_text(t.id.clone());
            let seen = db
                .query_with("SELECT 1 FROM transfers WHERE id = ?", [id.clone()])
                .await?;
            if !seen.is_empty() {
                return Ok(false);
            }
            db.execute_with(
                "INSERT INTO transfers VALUES(?, ?, ?, ?)",
                [
                    id,
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
    }

    /// Totals, and accounts that disagree with the recorded transfers.
    #[handler]
    async fn audit(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<Audit>> {
        let audit = ctx
            .with_sql(async |db| {
                let integrity = db.query("PRAGMA integrity_check").await?;
                if integrity.len() != 1 || integrity[0][0].to_string() != "ok" {
                    return Err("integrity check failed".into());
                }
                let rows = db
                    .query(
                        "SELECT (SELECT count(*) FROM accounts), (SELECT sum(balance) FROM accounts),
                                (SELECT count(*) FROM transfers),
                                (SELECT count(*) FROM accounts a WHERE a.balance != a.opening
                                    - (SELECT coalesce(sum(amount), 0) FROM transfers WHERE src = a.id)
                                    + (SELECT coalesce(sum(amount), 0) FROM transfers WHERE dst = a.id))",
                    )
                    .await?;
                let int = |i: usize| rows[0][i].as_int().unwrap_or(0);
                Ok(Audit {
                    accounts: int(0),
                    total: int(1),
                    transfers: int(2),
                    inconsistent: int(3),
                })
            })
            .await?;
        Ok(Json(audit))
    }
}

/// Abort probability per state request, in per mille.
static ABORT_PER_MILLE: AtomicU64 = AtomicU64::new(0);

fn with_aborts(state: Arc<ObjectState>) -> Arc<dyn KvStore> {
    Arc::new(Abort { inner: state })
}

/// Aborts the process before or after a random state request.
struct Abort {
    inner: Arc<ObjectState>,
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
        if x % 1000 < ABORT_PER_MILLE.load(Ordering::Relaxed) {
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
    let mut sql = Sql::new().schema("Ledger", SCHEMA);
    if let Ok(dir) = std::env::var("LEDGER_CACHE_DIR") {
        let config = PageCacheConfig::new(dir, 64 << 20, 256 << 20);
        sql = sql.cache(PageCache::open(config).await.expect("page cache"));
    }
    if abort_per_mille > 0 {
        ABORT_PER_MILLE.store(abort_per_mille, Ordering::Relaxed);
        sql = sql.wrap_store(with_aborts);
    }
    sql.install();
    let mut options = service_options()
        .retry_policy_initial_interval(Duration::from_millis(50))
        .retry_policy_max_interval(Duration::from_millis(500));
    if std::env::var_os("LEDGER_KEEP_JOURNAL").is_some() {
        // To inspect what invocations journaled.
        options = options.journal_retention(Duration::from_secs(3600));
    }
    let endpoint = Endpoint::builder()
        .bind(Ledger.into_service_definition().options(options))
        .build();
    // Rebind at once after a crash, past connections in TIME_WAIT.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind(([127, 0, 0, 1], port).into()).unwrap();
    HttpServer::new(endpoint)
        .serve(socket.listen(1024).unwrap())
        .await;
}

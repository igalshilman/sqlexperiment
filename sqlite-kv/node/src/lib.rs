//! Node.js bindings: SQL over a key/value store implemented in JavaScript.
//!
//! The host passes one dispatcher function, `([op, key, value])`, which `index.js`
//! builds from a store object `{ get, set, clear, sync }`. Every request goes
//! through it, so they reach the store in the order they were issued: a write
//! before a later read of its key, and every write before a `sync`.
//!
//! The store is ordered ([`KvStore::ordered`]): `set` and `clear` return
//! nothing, and are acknowledged once queued; `sync` resolves once every write
//! so far is stored. A statement resolves once its writes have reached the
//! store (a barrier through the same queue), so what its caller does next
//! comes after them. A process-wide page cache is built in, and
//! [`Database::save`] confirms a session's writes before the cache trusts them.
use bytes::Bytes;
use futures::future::BoxFuture;
use napi::bindgen_prelude::*;
use napi::sys;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use sqlite_kv::kv::KvStore;
use sqlite_kv::{Durability, PageCache, PageCacheConfig, Session, StorageOptions, Value};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, OnceCell};
use turso_core::Numeric;

const GET: u32 = 0;
const SET: u32 = 1;
const CLEAR: u32 = 2;
const SYNC: u32 = 3;
const BARRIER: u32 = 4;

type Args = (u32, String, Option<Buffer>);
type Dispatch = ThreadsafeFunction<Args, Promise<Option<Uint8Array>>, Args, Status, false, true>;

/// The JavaScript store.
struct JsStore {
    dispatch: Dispatch,
    /// Writes were queued since the last barrier.
    queued: AtomicBool,
}

fn failed(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl JsStore {
    /// Queue a write; it is acknowledged once queued.
    fn write(&self, op: u32, key: &str, value: Option<Buffer>) -> io::Result<()> {
        self.queued.store(true, Ordering::Release);
        match self.dispatch.call(
            (op, key.to_owned(), value),
            ThreadsafeFunctionCallMode::NonBlocking,
        ) {
            Status::Ok => Ok(()),
            status => Err(failed(format!("store call failed: {status}"))),
        }
    }

    /// Wait until the store has every write queued so far. The writes and a
    /// statement's result reach JavaScript through different queues, and
    /// Node.js dispatches a bounded batch of a queue per turn: without this,
    /// a statement could resolve before its commit's writes reached the store,
    /// and what the caller did next would come before them.
    async fn barrier(&self) -> io::Result<()> {
        if self.queued.swap(false, Ordering::AcqRel) {
            self.request(BARRIER, "").await?;
        }
        Ok(())
    }

    async fn request(&self, op: u32, key: &str) -> io::Result<Option<Bytes>> {
        let promise = self
            .dispatch
            .call_async_catch((op, key.to_owned(), None))
            .await
            .map_err(failed)?;
        let value = promise.await.map_err(failed)?;
        Ok(value.map(|value| Bytes::copy_from_slice(&value)))
    }
}

impl KvStore for JsStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(self.request(GET, key))
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        let result = self.write(SET, key, Some(Buffer::from(value.to_vec())));
        Box::pin(std::future::ready(result))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        let result = self.write(CLEAR, key, None);
        Box::pin(std::future::ready(result))
    }

    /// A read returns stored values, or this request's own writes.
    fn durable_when_visible(&self) -> bool {
        true
    }

    fn ordered(&self) -> bool {
        true
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { self.request(SYNC, "").await.map(drop) })
    }
}

/// The page cache shared by the process's databases: 256 MiB of memory and
/// 1 GiB of disk, in a file under the temporary directory, or the file
/// `SQLITE_KV_CACHE_FILE` names.
static CACHE: OnceCell<PageCache> = OnceCell::const_new();

async fn page_cache() -> Result<&'static PageCache> {
    CACHE
        .get_or_try_init(async || {
            let named = std::env::var_os("SQLITE_KV_CACHE_FILE").map(PathBuf::from);
            let file = named.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("sqlite-kv-{}.cache", std::process::id()))
            });
            let cache = PageCache::open(PageCacheConfig::new(&file, 256 << 20, 1 << 30)).await?;
            // The cache keeps the file open, and nothing else uses the
            // temporary one: unlinked now, it goes away with the process
            // however it ends, rather than staying behind (where unlinking an
            // open file is refused, it stays).
            if named.is_none() {
                let _ = std::fs::remove_file(&file);
            }
            Ok::<_, io::Error>(cache)
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))
}

fn sql_error(error: Box<dyn std::error::Error + Send + Sync>) -> Error {
    Error::from_reason(error.to_string())
}

fn sql_error_io(error: io::Error) -> Error {
    Error::from_reason(error.to_string())
}

/// One session of a namespace's database.
#[napi]
pub struct Database {
    session: Arc<Mutex<Option<Session>>>,
    store: Arc<JsStore>,
}

/// Open the database of `namespace` over the store behind `dispatch`; see
/// `index.js` for the store object it is built from.
#[napi(ts_return_type = "Promise<Database>")]
pub fn open<'env>(
    env: &'env Env,
    namespace: String,
    dispatch: Function<'env, Args, Promise<Option<Uint8Array>>>,
) -> Result<PromiseRaw<'env, Database>> {
    let dispatch: Dispatch = dispatch
        .build_threadsafe_function()
        .callee_handled::<false>()
        .weak::<true>()
        .build()?;
    let store = Arc::new(JsStore {
        dispatch,
        queued: AtomicBool::new(false),
    });
    env.spawn_future(async move {
        let options = StorageOptions {
            cache: Some(page_cache().await?.namespace(&namespace)),
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let session = Session::open_with_options(store.clone(), options)
            .await
            .map_err(sql_error)?;
        store.barrier().await.map_err(sql_error_io)?;
        Ok(Database {
            session: Arc::new(Mutex::new(Some(session))),
            store,
        })
    })
}

#[napi]
impl Database {
    /// Run SQL (one or more statements; parameters bind to a single one) and
    /// return the result rows. Each commit is durable once ordered: its writes
    /// have reached the store when this resolves.
    #[napi(
        ts_args_type = "sql: string, params?: SqlValue[]",
        ts_return_type = "Promise<SqlValue[][]>"
    )]
    pub async fn query(
        &self,
        sql: String,
        params: Option<Vec<SqlValue>>,
    ) -> Result<Vec<Vec<SqlValue>>> {
        let mut session = self.session.lock().await;
        let session = session
            .as_mut()
            .ok_or_else(|| Error::from_reason("the database is closed"))?;
        let rows = match params {
            Some(params) => {
                let params = params.into_iter().map(|p| p.0);
                session.query_with(&sql, params).await
            }
            None => session.query(&sql).await,
        };
        self.store.barrier().await.map_err(sql_error_io)?;
        let rows = rows.map_err(sql_error)?;
        Ok(rows
            .into_iter()
            .map(|row| row.into_iter().map(SqlValue).collect())
            .collect())
    }

    /// [`Self::query`], also returning the column names of the statement whose
    /// rows these are.
    #[napi(
        ts_args_type = "sql: string, params?: SqlValue[]",
        ts_return_type = "Promise<{ columns: string[]; rows: SqlValue[][] }>"
    )]
    pub async fn query_columns(
        &self,
        sql: String,
        params: Option<Vec<SqlValue>>,
    ) -> Result<QueryResult> {
        let mut session = self.session.lock().await;
        let session = session
            .as_mut()
            .ok_or_else(|| Error::from_reason("the database is closed"))?;
        let params = params.map(|params| params.into_iter().map(|p| p.0).collect());
        let result = session.query_columns(&sql, params).await;
        self.store.barrier().await.map_err(sql_error_io)?;
        let (columns, rows) = result.map_err(sql_error)?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(SqlValue).collect())
                .collect(),
        })
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error ends it.
    #[napi]
    pub fn is_usable(&self) -> bool {
        self.session
            .try_lock()
            .is_ok_and(|session| session.as_ref().is_some_and(Session::is_usable))
    }

    /// End the session, confirming its writes (one `sync` if it wrote) so that
    /// the page cache may trust them.
    #[napi]
    pub async fn save(&self) -> Result<()> {
        let session = self.session.lock().await.take();
        let session = session.ok_or_else(|| Error::from_reason("the database is closed"))?;
        session.save().await.map(drop).map_err(sql_error)
    }

    /// End the session without confirming its writes: the next session in
    /// this process starts cold.
    #[napi]
    pub async fn close(&self) {
        self.session.lock().await.take();
    }
}

/// Rows with the column names of their statement.
#[napi(object)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

/// An SQL value: `null`, a number (an integer is stored as INTEGER), a bigint,
/// a string, a `Uint8Array` (BLOB), or a boolean (0 or 1).
pub struct SqlValue(Value);

impl TypeName for SqlValue {
    fn type_name() -> &'static str {
        "SqlValue"
    }

    fn value_type() -> ValueType {
        ValueType::Unknown
    }
}

impl ValidateNapiValue for SqlValue {}

impl FromNapiValue for SqlValue {
    unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
        let unknown = unsafe { Unknown::from_napi_value(env, value)? };
        let value = match unknown.get_type()? {
            ValueType::Null | ValueType::Undefined => Value::Null,
            ValueType::Boolean => {
                Value::from_i64(i64::from(unsafe { bool::from_napi_value(env, value)? }))
            }
            ValueType::Number => {
                let number = unsafe { f64::from_napi_value(env, value)? };
                if number.fract() == 0.0 && number.abs() < 9.2e18 {
                    Value::from_i64(number as i64)
                } else {
                    Value::from_f64(number)
                }
            }
            ValueType::BigInt => {
                let (number, lossless) = unsafe { BigInt::from_napi_value(env, value)? }.get_i64();
                if !lossless {
                    return Err(Error::from_reason("a bigint parameter exceeds 64 bits"));
                }
                Value::from_i64(number)
            }
            ValueType::String => Value::build_text(unsafe { String::from_napi_value(env, value)? }),
            ValueType::Object => {
                let bytes = unsafe { Uint8Array::from_napi_value(env, value)? };
                Value::from_slice(&bytes).map_err(|e| Error::from_reason(e.to_string()))?
            }
            other => {
                return Err(Error::from_reason(format!(
                    "unsupported SQL value: {other}"
                )));
            }
        };
        Ok(SqlValue(value))
    }
}

impl ToNapiValue for SqlValue {
    unsafe fn to_napi_value(env: sys::napi_env, value: Self) -> Result<sys::napi_value> {
        const SAFE: i64 = (1 << 53) - 1;
        unsafe {
            match value.0 {
                Value::Null => Null::to_napi_value(env, Null),
                Value::Numeric(Numeric::Integer(i)) if (-SAFE..=SAFE).contains(&i) => {
                    f64::to_napi_value(env, i as f64)
                }
                Value::Numeric(Numeric::Integer(i)) => i64n::to_napi_value(env, i64n(i)),
                Value::Numeric(Numeric::Float(f)) => f64::to_napi_value(env, f64::from(f)),
                Value::Text(text) => <&str>::to_napi_value(env, text.as_str()),
                Value::Blob(blob) => Buffer::to_napi_value(env, Buffer::from(blob.to_vec())),
            }
        }
    }
}

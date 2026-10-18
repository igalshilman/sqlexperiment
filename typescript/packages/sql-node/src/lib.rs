//! Node.js bindings: SQL over a key/value store implemented in JavaScript.
//!
//! The host passes one dispatcher function, `([op, key, value])`, which `index.js`
//! builds from a store object `{ get, set, clear, sync }`. Every request goes
//! through it in the order it was issued, so a read sees the writes before it.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`sqlite_kv::kv`]): a statement that
//! commits queues its writes, then resolves at one `sync`. A process-wide
//! page cache is built in.
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

type Args = (u32, String, Option<Buffer>);
type Dispatch = ThreadsafeFunction<Args, Promise<Option<Uint8Array>>, Args, Status, false, true>;

/// The JavaScript store's queue.
struct JsQueue {
    dispatch: Dispatch,
}

fn failed(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl JsQueue {
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

impl JsQueue {
    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()> {
        let (op, value) = match value {
            Some(value) => (SET, Some(Buffer::from(value.to_vec()))),
            None => (CLEAR, None),
        };
        match self.dispatch.call(
            (op, key.to_owned(), value),
            ThreadsafeFunctionCallMode::NonBlocking,
        ) {
            Status::Ok => Ok(()),
            status => Err(failed(format!("store call failed: {status}"))),
        }
    }
}

impl KvStore for JsQueue {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(self.request(GET, key))
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.queue(key, Some(value))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.queue(key, None)
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

/// One session of a namespace's database.
#[napi]
pub struct Database {
    session: Arc<Mutex<Option<Session>>>,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: Arc<AtomicBool>,
}

/// Open the database of `namespace` over the store behind `dispatch`; see
/// `index.js` for the store object it is built from.
///
/// A `reader` reads the namespace beside its owner (see `Session::open_reader`):
/// it writes nothing to the store, and `isCurrent` says whether what it read
/// is still the stored state.
#[napi(ts_return_type = "Promise<Database>")]
pub fn open<'env>(
    env: &'env Env,
    namespace: String,
    dispatch: Function<'env, Args, Promise<Option<Uint8Array>>>,
    reader: Option<bool>,
) -> Result<PromiseRaw<'env, Database>> {
    let dispatch: Dispatch = dispatch
        .build_threadsafe_function()
        .callee_handled::<false>()
        .weak::<true>()
        .build()?;
    let store = Arc::new(JsQueue { dispatch });
    env.spawn_future(async move {
        let cache = match reader.unwrap_or(false) {
            true => None,
            false => Some(page_cache().await?.namespace(&namespace)),
        };
        let options = StorageOptions {
            cache,
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let session = match reader.unwrap_or(false) {
            true => Session::open_reader(store.clone(), options).await,
            false => Session::open_with_options(store.clone(), options).await,
        }
        .map_err(sql_error)?;
        Ok(Database {
            session: Arc::new(Mutex::new(Some(session))),
            usable: Arc::new(AtomicBool::new(true)),
        })
    })
}

#[napi]
impl Database {
    /// Run SQL (one or more statements; parameters bind to a single one) and
    /// return the result rows. A commit is durable when this resolves.
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
        self.usable.store(session.is_usable(), Ordering::Release);
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
        self.usable.store(session.is_usable(), Ordering::Release);
        let (columns, rows) = result.map_err(sql_error)?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(SqlValue).collect())
                .collect(),
        })
    }

    /// `COMMIT` the open transaction; with `effects`, they commit with it
    /// (see `Session::commit_with`).
    #[napi]
    pub async fn commit(&self, effects: Option<Buffer>) -> Result<()> {
        let mut session = self.session.lock().await;
        let session = session
            .as_mut()
            .ok_or_else(|| Error::from_reason("the database is closed"))?;
        let result = match effects {
            Some(effects) => session.commit_with(Bytes::copy_from_slice(&effects)).await,
            None => session.execute("COMMIT").await,
        };
        self.usable.store(session.is_usable(), Ordering::Release);
        result.map_err(sql_error)
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error ends it. A call that is running does not change the
    /// answer until it ends.
    #[napi]
    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them (see `Session::effects`).
    #[napi]
    pub async fn effects(&self) -> Result<Buffer> {
        let session = self.session.lock().await;
        let session = session
            .as_ref()
            .ok_or_else(|| Error::from_reason("the database is closed"))?;
        Ok(Buffer::from(session.effects().to_vec()))
    }

    /// Whether the store still holds the state this reader opened on, so
    /// that what it read is: if not, read again in a new session (see
    /// `Session::is_current`).
    #[napi]
    pub async fn is_current(&self) -> Result<bool> {
        let session = self.session.lock().await;
        let session = session
            .as_ref()
            .ok_or_else(|| Error::from_reason("the database is closed"))?;
        session.is_current().await.map_err(sql_error)
    }

    /// End the session, leaving its engine to the next one.
    #[napi]
    pub async fn save(&self) -> Result<()> {
        let session = self.session.lock().await.take();
        self.usable.store(false, Ordering::Release);
        let session = session.ok_or_else(|| Error::from_reason("the database is closed"))?;
        session.save().await.map(drop).map_err(sql_error)
    }

    /// End the session.
    #[napi]
    pub async fn close(&self) {
        self.session.lock().await.take();
        self.usable.store(false, Ordering::Release);
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

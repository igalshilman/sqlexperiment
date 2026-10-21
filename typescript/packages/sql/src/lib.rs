//! Node.js bindings: SQL over a key/value store implemented in JavaScript.
//!
//! The host passes one dispatcher function, `([op, key, value])`, which `index.js`
//! builds from a store object `{ get, set, clear, sync }`. Every request goes
//! through it in the order it was issued, so a read sees the writes before it.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`journal::kv`]): a statement that
//! commits queues its writes, then resolves at one `sync`. A process-wide
//! page cache is built in.
use bytes::Bytes;
use futures::future::BoxFuture;
use journal::kv::KvStore;
use journal::{Journal, PageCache, PageCacheConfig, StorageOptions};
use napi::bindgen_prelude::*;
use napi::sys;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use sql::{Sql, Value};
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
/// `RESTATE_SQL_CACHE_FILE` names.
static CACHE: OnceCell<PageCache> = OnceCell::const_new();

async fn page_cache() -> Result<&'static PageCache> {
    CACHE
        .get_or_try_init(async || {
            let named = std::env::var_os("RESTATE_SQL_CACHE_FILE").map(PathBuf::from);
            let file = named.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("restate-sql-{}.cache", std::process::id()))
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

/// One session of a namespace's database: SQL on the namespace's journal,
/// for an object's handler without Restate's journal. Every SQL commit is
/// durable when its call returns: a call that leaves no SQL transaction open
/// commits the journal.
#[napi]
pub struct Database {
    session: Arc<Mutex<Option<Sql>>>,
    state: Arc<State>,
}

/// Open the database of `namespace` over the store behind `dispatch`; see
/// `index.js` for the store object it is built from.
///
/// A `reader` reads the namespace beside its owner (see `Journal::open_reader`):
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
        let reader = reader.unwrap_or(false);
        let cache = match reader {
            true => None,
            false => Some(page_cache().await?.namespace(&namespace)),
        };
        let options = StorageOptions {
            cache,
            ..StorageOptions::default()
        };
        let journal = match reader {
            true => Journal::open_reader(store.clone(), options).await,
            false => Journal::open_with_options(store.clone(), options).await,
        }
        .map_err(sql_error)?;
        let session = Sql::open(&journal).await.map_err(sql_error)?;
        Ok(Database {
            session: Arc::new(Mutex::new(Some(session))),
            state: Arc::new(State::new(reader)),
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
        let mut open = self.session.lock().await;
        let session = open.as_mut().ok_or_else(closed)?;
        let rows = match params {
            Some(params) => {
                let params = params.into_iter().map(|p| p.0);
                session.query_with(&sql, params).await
            }
            None => session.query(&sql).await,
        };
        let rows = settle(&mut open, &self.state, rows).await;
        Ok(rows
            .map_err(sql_error)?
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
        let mut open = self.session.lock().await;
        let session = open.as_mut().ok_or_else(closed)?;
        let params = params.map(|params| params.into_iter().map(|p| p.0).collect());
        let result = session.query_columns(&sql, params).await;
        let (columns, rows) = settle(&mut open, &self.state, result)
            .await
            .map_err(sql_error)?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(SqlValue).collect())
                .collect(),
        })
    }

    /// `BEGIN` the transaction of a `transaction()` body: until its `commit`
    /// or `rollback`, a statement that ends it ends the session.
    #[napi]
    pub async fn begin(&self) -> Result<()> {
        let mut open = self.session.lock().await;
        let session = open.as_mut().ok_or_else(closed)?;
        let result = session.execute("BEGIN").await;
        if result.is_ok() {
            self.state.body.store(true, Ordering::Release);
        }
        settle(&mut open, &self.state, result)
            .await
            .map_err(sql_error)
    }

    /// `COMMIT` the transaction of a `transaction()` body; with `effects`,
    /// they commit with it, as the record `effects`, in the same put, even if
    /// the transaction changed nothing. A `COMMIT` that fails leaves the
    /// transaction open, for `rollback`.
    #[napi]
    pub async fn commit(&self, effects: Option<Buffer>) -> Result<()> {
        let mut open = self.session.lock().await;
        let session = open.as_mut().ok_or_else(closed)?;
        self.state.body.store(false, Ordering::Release);
        let effects = effects.map(|effects| Bytes::copy_from_slice(&effects));
        let result = commit(session, self.state.reader, effects).await;
        settle(&mut open, &self.state, result)
            .await
            .map_err(sql_error)
    }

    /// `ROLLBACK` the transaction of a `transaction()` body, if it is still
    /// open.
    #[napi]
    pub async fn rollback(&self) -> Result<()> {
        let mut open = self.session.lock().await;
        let session = open.as_mut().ok_or_else(closed)?;
        self.state.body.store(false, Ordering::Release);
        let result = rollback(session).await;
        settle(&mut open, &self.state, result)
            .await
            .map_err(sql_error)
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error ends it. A call that is running does not change the
    /// answer until it ends.
    #[napi]
    pub fn is_usable(&self) -> bool {
        self.state.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them.
    #[napi]
    pub async fn effects(&self) -> Result<Buffer> {
        let open = self.session.lock().await;
        let session = open.as_ref().ok_or_else(closed)?;
        Ok(Buffer::from(
            session.journal().record(EFFECTS).get().to_vec(),
        ))
    }

    /// Whether the store still holds the state this reader opened on, so
    /// that what it read is: if not, read again in a new session (see
    /// `Journal::is_current`).
    #[napi]
    pub async fn is_current(&self) -> Result<bool> {
        let open = self.session.lock().await;
        let session = open.as_ref().ok_or_else(closed)?;
        session.is_current().await.map_err(sql_error)
    }

    /// End the session after the calls before it, leaving its engine to the
    /// next one. It fails if they left a transaction open, whose changes are
    /// lost, or a failure ended the session.
    #[napi]
    pub async fn save(&self) -> Result<()> {
        let session = self.session.lock().await.take();
        self.state.usable.store(false, Ordering::Release);
        save(session.ok_or_else(closed)?).map_err(sql_error)
    }

    /// End the session, whatever state it is in.
    #[napi]
    pub async fn close(&self) {
        self.session.lock().await.take();
        self.state.usable.store(false, Ordering::Release);
    }
}

fn closed() -> Error {
    Error::from_reason("the database is closed")
}

/// The record a commit's effects are in.
const EFFECTS: &str = "effects";

type SqlResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// What a session's calls share besides the session.
struct State {
    /// A reader's session: it never commits.
    reader: bool,
    /// Inside a `transaction()` body, from `begin` to its `commit` or
    /// `rollback`.
    body: AtomicBool,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: AtomicBool,
}

impl State {
    fn new(reader: bool) -> Self {
        Self {
            reader,
            body: AtomicBool::new(false),
            usable: AtomicBool::new(true),
        }
    }
}

/// End a call. A statement that ended the transaction of a `transaction()`
/// body ends the session, rather than commit the body halfway. Otherwise, a
/// call that leaves no SQL transaction open commits the journal: what the SQL
/// committed, with the effects set, durable before the call returns. A
/// reader commits nothing, and a session that a failure ended cannot.
async fn settle<T>(open: &mut Option<Sql>, state: &State, result: SqlResult<T>) -> SqlResult<T> {
    let Some(session) = open.as_mut() else {
        return result;
    };
    if state.body.load(Ordering::Acquire) && !session.in_transaction() {
        state.body.store(false, Ordering::Release);
        state.usable.store(false, Ordering::Release);
        open.take();
        return Err("a statement ended the transaction of transaction(), whose body must not: the session has ended".into());
    }
    // Even after an SQL error: the statements before it stand.
    let committed = match !state.reader && session.is_usable() && !session.in_transaction() {
        true => commit_journal(session).await,
        false => Ok(()),
    };
    state.usable.store(session.is_usable(), Ordering::Release);
    committed.and(result)
}

async fn commit_journal(session: &mut Sql) -> SqlResult<()> {
    session.flush().await?;
    session.journal().commit().await?;
    Ok(())
}

/// `COMMIT`, then set `effects`: a `COMMIT` that fails sets none.
async fn commit(session: &mut Sql, reader: bool, effects: Option<Bytes>) -> SqlResult<()> {
    if let Some(effects) = &effects {
        if reader {
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
}

/// `ROLLBACK` the open transaction, unless a failure ended the session.
async fn rollback(session: &mut Sql) -> SqlResult<()> {
    match session.is_usable() && session.in_transaction() {
        true => session.execute("ROLLBACK").await,
        false => Ok(()),
    }
}

/// End `session`, which leaves its engine to the next one when it is clean.
fn save(session: Sql) -> SqlResult<()> {
    if !session.is_usable() {
        return Err("a failure ended the session".into());
    }
    if session.in_transaction() {
        return Err("a transaction was left open: its changes are lost".into());
    }
    Ok(())
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

//! Node.js bindings: SQL over a key/value store implemented in JavaScript.
//!
//! The host passes one dispatcher function, `([op, key, value])`, which `index.js`
//! builds from a store object `{ get, set, clear, sync }`. Every request goes
//! through it in the order it was issued, so a read sees the writes before it.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`state::kv`]): a statement that
//! commits queues its writes, then resolves at one `sync`. The session itself,
//! its commits and the process-wide page cache, is [`sql::binding::Session`].
use bytes::Bytes;
use futures::future::BoxFuture;
use napi::bindgen_prelude::*;
use napi::sys;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use sql::binding::Session;
use sql::{Numeric, Value};
use state::kv::KvStore;
use std::io;
use std::sync::Arc;

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

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl JsQueue {
    /// A request that answers: a read, or a sync.
    async fn request(&self, op: u32, key: &str) -> io::Result<Option<Bytes>> {
        let promise = self
            .dispatch
            .call_async_catch((op, key.to_owned(), None))
            .await
            .map_err(io_error)?;
        let value = promise.await.map_err(io_error)?;
        Ok(value.map(|value| Bytes::copy_from_slice(&value)))
    }

    /// Queue a write: a `set`, or a `clear` when `value` is `None`.
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
            status => Err(io_error(format!("store call failed: {status}"))),
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

fn error(error: impl std::fmt::Display) -> Error {
    Error::from_reason(error.to_string())
}

/// One session of a namespace's database (see [`Session`]): every SQL commit
/// is durable when its call resolves.
#[napi]
pub struct Database(Arc<Session>);

/// Open the database of `namespace` over the store behind `dispatch`; see
/// `index.js` for the store object it is built from, and [`Session::open`]
/// for a `reader`.
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
        let session = Session::open(store, &namespace, reader.unwrap_or(false)).await;
        Ok(Database(Arc::new(session.map_err(error)?)))
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
        Ok(self.query_columns(sql, params).await?.rows)
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
        let params = params.map(|params| params.into_iter().map(|p| p.0).collect());
        let (columns, rows) = self.0.query(&sql, params).await.map_err(error)?;
        Ok(QueryResult {
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(SqlValue).collect())
                .collect(),
        })
    }

    /// See [`Session::begin`].
    #[napi]
    pub async fn begin(&self) -> Result<()> {
        self.0.begin().await.map_err(error)
    }

    /// See [`Session::commit`].
    #[napi]
    pub async fn commit(&self, effects: Option<Buffer>) -> Result<()> {
        let effects = effects.map(|effects| Bytes::copy_from_slice(&effects));
        self.0.commit(effects).await.map_err(error)
    }

    /// See [`Session::rollback`].
    #[napi]
    pub async fn rollback(&self) -> Result<()> {
        self.0.rollback().await.map_err(error)
    }

    /// See [`Session::is_usable`].
    #[napi]
    pub fn is_usable(&self) -> bool {
        self.0.is_usable()
    }

    /// See [`Session::effects`].
    #[napi]
    pub async fn effects(&self) -> Result<Buffer> {
        let effects = self.0.effects().await.map_err(error)?;
        Ok(Buffer::from(effects.to_vec()))
    }

    /// See [`Session::is_current`].
    #[napi]
    pub async fn is_current(&self) -> Result<bool> {
        self.0.is_current().await.map_err(error)
    }

    /// See [`Session::save`].
    #[napi]
    pub async fn save(&self) -> Result<()> {
        self.0.save().await.map_err(error)
    }

    /// See [`Session::close`].
    #[napi]
    pub async fn close(&self) {
        self.0.close().await
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

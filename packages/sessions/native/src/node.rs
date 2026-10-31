//! Node.js bindings: SQL over a key/value store implemented in JavaScript.
//!
//! `open` takes the store, an object `{ get, set, clear, sync, keys? }`
//! (`Store` in `src/database.ts`). Every request goes to it through one
//! queue, in the order it was issued, so a read sees the writes before it:
//! Rust reaches JavaScript only through a thread-safe function, and each such
//! function is a queue of its own, with no order across them. So there is
//! one, [`dispatcher`], which calls the store's method a request names.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`KvStore`]): a statement that commits
//! queues its writes, then resolves at one `sync`. With `keys`, a checkpoint
//! deletes the packs no checkpoint needs; a store without keeps its garbage.
//!
//! Every database of the process is kept warm in one page cache
//! ([`page_cache`]); a session itself, and its commits, is a
//! [`crate::Database`].
//!
//! A call runs on the JavaScript thread as far as it can ([`answer`]): one
//! that needs nothing from the store, such as SQL on cached pages or a
//! statement inside a transaction, returns its value, or throws its error;
//! one that waits for the store goes on in the runtime and returns a promise.
//! The caller `await`s either.
use crate::{KvStore, Numeric, PageCache, PageCacheConfig, Value};
use bytes::Bytes;
use futures::future::BoxFuture;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{JsValue, sys};
use napi_derive::napi;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

/// What a request to the store does: the store's method it calls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Get = 0,
    Set = 1,
    Clear = 2,
    Sync = 3,
    Keys = 4,
}

impl Op {
    fn from_code(code: u32) -> Option<Op> {
        [Op::Get, Op::Set, Op::Clear, Op::Sync, Op::Keys]
            .into_iter()
            .find(|op| *op as u32 == code)
    }

    fn method(self) -> &'static str {
        match self {
            Op::Get => "get",
            Op::Set => "set",
            Op::Clear => "clear",
            Op::Sync => "sync",
            Op::Keys => "keys",
        }
    }
}

/// A request, as the dispatcher's arguments: `(op, key, value)`.
type Args = FnArgs<(u32, String, Option<Buffer>)>;
/// What a request answers: a `get`'s value, the store's keys, or nothing.
type Reply = Option<Either<Uint8Array, Vec<String>>>;
type Dispatch = ThreadsafeFunction<Args, Promise<Reply>, Args, Status, false, true>;

/// The queue that every request to `store` goes through: one function, run
/// on the JavaScript thread, that calls the store's method for the request,
/// with the store as `this`; a method that answers may answer a value or a
/// promise of it. A failed write cannot fail its caller, which has moved on:
/// the next `sync`, and every one after it, rejects with its error.
fn dispatcher(env: &Env, store: Object) -> Result<Dispatch> {
    let failure = RefCell::new(None::<Error>);
    let dispatch = env.create_function_from_closure::<Args, _, _>("dispatch", move |ctx| {
        let store: Object = ctx.this()?;
        let code: u32 = ctx.get(0)?;
        let op = Op::from_code(code)
            .ok_or_else(|| Error::from_reason(format!("unknown store operation {code}")))?;
        let arg = |index: usize| ctx.get::<Unknown>(index).map(|value| value.raw());
        let args = match op {
            Op::Set => vec![arg(1)?, arg(2)?],
            Op::Get | Op::Clear => vec![arg(1)?],
            Op::Sync | Op::Keys => vec![],
        };
        if op == Op::Sync
            && let Some(error) = &*failure.borrow()
        {
            return Err(error.try_clone()?);
        }
        let result = call(&store, op.method(), &args);
        if op == Op::Set || op == Op::Clear {
            if let Err(error) = result {
                failure.borrow_mut().get_or_insert(error);
            }
            return unsafe { <()>::to_napi_value(ctx.env.raw(), ()) };
        }
        // `Promise`, a function, as the object `resolve` is called on.
        let promise: Object = ctx
            .env
            .get_global()?
            .get_named_property_unchecked("Promise")?;
        call(&promise, "resolve", &[result?])
    })?;
    // Typed as the queue calls it: each method it awaits answers a promise.
    let dispatch =
        unsafe { Function::<Args, Promise<Reply>>::from_napi_value(env.raw(), dispatch.raw()) }?;
    dispatch
        .bind(store)?
        .build_threadsafe_function()
        .callee_handled::<false>()
        .weak::<true>()
        .build()
}

/// `object[name](...args)`, with the object as `this`; a throw is its error.
fn call(object: &Object, name: &str, args: &[sys::napi_value]) -> Result<sys::napi_value> {
    let method: Unknown = object.get_named_property(name)?;
    let env = object.value().env;
    let mut result = std::ptr::null_mut();
    napi::check_pending_exception!(env, unsafe {
        sys::napi_call_function(
            env,
            object.raw(),
            method.raw(),
            args.len(),
            args.as_ptr(),
            &mut result,
        )
    })?;
    Ok(result)
}

/// The JavaScript store's queue.
struct JsQueue {
    dispatch: Dispatch,
    /// Whether the store has `keys`.
    lists_keys: bool,
}

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl JsQueue {
    /// A request that answers: a read, a sync, or the keys.
    async fn request(&self, op: Op, key: &str) -> io::Result<Reply> {
        let promise = self
            .dispatch
            .call_async_catch((op as u32, key.to_owned(), None).into())
            .await
            .map_err(io_error)?;
        promise.await.map_err(io_error)
    }

    /// Queue a write: a `set`, or a `clear` when `value` is `None`.
    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()> {
        let (op, value) = match value {
            Some(value) => (Op::Set, Some(Buffer::from(value.to_vec()))),
            None => (Op::Clear, None),
        };
        match self.dispatch.call(
            (op as u32, key.to_owned(), value).into(),
            ThreadsafeFunctionCallMode::NonBlocking,
        ) {
            Status::Ok => Ok(()),
            status => Err(io_error(format!("store call failed: {status}"))),
        }
    }
}

fn unexpected(op: Op) -> io::Error {
    io_error(format!(
        "the store's {} answered with the wrong type",
        op.method()
    ))
}

impl KvStore for JsQueue {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            match self.request(Op::Get, key).await? {
                None => Ok(None),
                Some(Either::A(value)) => Ok(Some(Bytes::copy_from_slice(&value))),
                Some(Either::B(_)) => Err(unexpected(Op::Get)),
            }
        })
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.queue(key, Some(value))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.queue(key, None)
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { self.request(Op::Sync, "").await.map(drop) })
    }

    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        Box::pin(async move {
            if !self.lists_keys {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the store has no keys",
                ));
            }
            match self.request(Op::Keys, "").await? {
                Some(Either::B(keys)) => Ok(keys),
                _ => Err(unexpected(Op::Keys)),
            }
        })
    }
}

/// A failure of the database as JavaScript gets it: one a retry would repeat
/// ([`crate::Refused`]) has the code `InvalidArg`, which `src/database.ts`
/// tells apart, as it may have ended the session.
fn error(error: Box<dyn std::error::Error + Send + Sync>) -> Error {
    match error.is::<crate::Refused>() {
        true => Error::new(Status::InvalidArg, error.to_string()),
        false => Error::from_reason(error.to_string()),
    }
}

/// One session of a namespace's database (see [`crate::Database`]): every
/// SQL commit is durable when its call resolves.
#[napi]
pub struct Database(Arc<crate::Database>);

/// `call`'s value if it is ready at once, polled here on the JavaScript
/// thread; else a promise of it, as it goes on in the runtime. Ready, a call
/// spares two thread hops and a thread-safe function: what a call to the
/// addon costs otherwise, several times the SQL it runs.
fn answer<'env, T>(
    env: &'env Env,
    call: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<Either<T, PromiseRaw<'env, T>>>
where
    T: ToNapiValue + Send + 'static,
{
    use std::task::{Context, Poll, Waker};
    let mut call = Box::pin(call);
    match call.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result.map(Either::A),
        Poll::Pending => env.spawn_future(call).map(Either::B),
    }
}

/// The page cache shared by the process's databases: 256 MiB of memory and
/// 1 GiB of disk, in a file under the temporary directory, or the file
/// `RESTATE_SQL_CACHE_FILE` names.
async fn page_cache() -> io::Result<PageCache> {
    static CACHE: futures::lock::Mutex<Option<PageCache>> = futures::lock::Mutex::new(None);
    let mut cache = CACHE.lock().await;
    if let Some(cache) = &*cache {
        return Ok(cache.clone());
    }
    let opened = PageCache::open(PageCacheConfig {
        file: std::env::var_os("RESTATE_SQL_CACHE_FILE").map(std::path::PathBuf::from),
        ..PageCacheConfig::new(256 << 20, 1 << 30)
    })
    .await?;
    Ok(cache.insert(opened).clone())
}

/// Open the database of `namespace` over `store` (see the module's doc),
/// kept warm in the process's page cache; see [`crate::Open::reader`] for a
/// `reader`.
#[napi(
    ts_args_type = "namespace: string, store: Store, reader?: boolean",
    ts_return_type = "Promise<Database>"
)]
pub(crate) fn open<'env>(
    env: &'env Env,
    namespace: String,
    store: Object<'env>,
    reader: Option<bool>,
) -> Result<PromiseRaw<'env, Database>> {
    let lists_keys =
        store.get_named_property::<Unknown>("keys")?.get_type()? == ValueType::Function;
    let dispatch = dispatcher(env, store)?;
    let store = Arc::new(JsQueue {
        dispatch,
        lists_keys,
    });
    env.spawn_future(async move {
        let mut open = crate::Database::open(store).cached(&page_cache().await?, &namespace);
        if reader == Some(true) {
            open = open.reader();
        }
        Ok(Database(Arc::new(open.await.map_err(error)?)))
    })
}

#[napi]
impl Database {
    /// Run SQL (one or more statements; parameters bind to a single one) and
    /// return the rows of its last statement, with its column names. A
    /// commit is durable when this resolves.
    #[napi(
        ts_args_type = "sql: string, params?: SqlValue[]",
        ts_return_type = "Answer<{ columns: string[]; rows: SqlValue[][] }>"
    )]
    pub fn query_columns<'env>(
        &self,
        env: &'env Env,
        sql: String,
        params: Option<Vec<SqlValue>>,
    ) -> Result<Either<QueryResult, PromiseRaw<'env, QueryResult>>> {
        let db = self.0.clone();
        answer(env, async move {
            let params = params.map(|params| params.into_iter().map(|p| p.0).collect());
            let (columns, rows) = db.query_columns(&sql, params).await.map_err(error)?;
            Ok(QueryResult {
                columns,
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(SqlValue).collect())
                    .collect(),
            })
        })
    }

    /// See [`crate::Database::begin`].
    #[napi(ts_return_type = "Answer<void>")]
    pub fn begin<'env>(&self, env: &'env Env) -> Result<Either<(), PromiseRaw<'env, ()>>> {
        let db = self.0.clone();
        answer(env, async move { db.begin().await.map_err(error) })
    }

    /// See [`crate::Database::commit`]: `records` by name.
    #[napi(
        ts_args_type = "records?: Record<string, Buffer>",
        ts_return_type = "Answer<void>"
    )]
    pub fn commit<'env>(
        &self,
        env: &'env Env,
        records: Option<HashMap<String, Buffer>>,
    ) -> Result<Either<(), PromiseRaw<'env, ()>>> {
        let records: Vec<_> = (records.unwrap_or_default().into_iter())
            .map(|(name, value)| (name, Bytes::copy_from_slice(&value)))
            .collect();
        let db = self.0.clone();
        answer(env, async move { db.commit(records).await.map_err(error) })
    }

    /// See [`crate::Database::rollback`].
    #[napi(ts_return_type = "Answer<void>")]
    pub fn rollback<'env>(&self, env: &'env Env) -> Result<Either<(), PromiseRaw<'env, ()>>> {
        let db = self.0.clone();
        answer(env, async move { db.rollback().await.map_err(error) })
    }

    /// See [`crate::Database::is_usable`].
    #[napi]
    pub fn is_usable(&self) -> bool {
        self.0.is_usable()
    }

    /// See [`crate::Database::record`].
    #[napi(ts_return_type = "Answer<Buffer>")]
    pub fn record<'env>(
        &self,
        env: &'env Env,
        name: String,
    ) -> Result<Either<Buffer, PromiseRaw<'env, Buffer>>> {
        let db = self.0.clone();
        answer(env, async move {
            let value = db.record(&name).await.map_err(error)?;
            Ok(Buffer::from(value.to_vec()))
        })
    }

    /// See [`crate::Database::save`].
    #[napi(ts_return_type = "Answer<void>")]
    pub fn save<'env>(&self, env: &'env Env) -> Result<Either<(), PromiseRaw<'env, ()>>> {
        let db = self.0.clone();
        answer(env, async move { db.save().await.map_err(error) })
    }

    /// See [`crate::Database::close`].
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

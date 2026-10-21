//! Python bindings: SQL over a key/value store implemented in Python.
//!
//! The host passes its asyncio event loop and one dispatcher function,
//! `dispatch(op, key, value, reply)`, which `sqlite_kv/__init__.py` builds from
//! a store object (`get`, `set`, `clear`, `sync`). Every request is scheduled
//! on the loop with `call_soon_threadsafe`, which runs callbacks in the order
//! they were scheduled, so a read sees the writes before it.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`journal::kv`]): a statement that
//! commits queues its writes, then completes at one `sync`. A process-wide
//! page cache is built in.
use bytes::Bytes;
use futures::future::BoxFuture;
use journal::kv::KvStore;
use journal::{Durability, PageCache, PageCacheConfig, Session, StorageOptions, Value};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyFloat, PyInt, PyList, PyString, PyTuple};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, OnceCell, oneshot};
use turso_core::Numeric;

const GET: u32 = 0;
const SET: u32 = 1;
const CLEAR: u32 = 2;
const SYNC: u32 = 3;

/// What a request's reply carries: the value read, or the store's error.
type Answer = Result<Option<Vec<u8>>, String>;

/// The callable that `dispatch` calls once with a request's outcome:
/// `reply(value)` or `reply(None, error)`.
#[pyclass(frozen)]
struct Reply {
    sender: std::sync::Mutex<Option<oneshot::Sender<Answer>>>,
}

#[pymethods]
impl Reply {
    #[pyo3(signature = (value, error=None))]
    fn __call__(&self, value: Option<Vec<u8>>, error: Option<String>) {
        let sender = self.sender.lock().map(|mut s| s.take()).ok().flatten();
        if let Some(sender) = sender {
            let _ = sender.send(match error {
                Some(error) => Err(error),
                None => Ok(value),
            });
        }
    }
}

/// The Python store, behind the dispatcher on its event loop.
struct PyStore {
    event_loop: Py<PyAny>,
    dispatch: Py<PyAny>,
}

fn failed(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl PyStore {
    fn schedule(
        &self,
        op: u32,
        key: &str,
        value: Option<Bytes>,
        reply: Option<Reply>,
    ) -> io::Result<()> {
        Python::attach(|py| {
            let value = value.map(|value| PyBytes::new(py, &value).into_any().unbind());
            let reply = match reply {
                Some(reply) => Py::new(py, reply)?.into_any(),
                None => py.None(),
            };
            self.event_loop.call_method1(
                py,
                "call_soon_threadsafe",
                (self.dispatch.clone_ref(py), op, key, value, reply),
            )?;
            Ok::<_, PyErr>(())
        })
        .map_err(failed)
    }

    async fn request(&self, op: u32, key: &str) -> io::Result<Option<Bytes>> {
        let (sender, receiver) = oneshot::channel();
        let reply = Reply {
            sender: std::sync::Mutex::new(Some(sender)),
        };
        self.schedule(op, key, None, Some(reply))?;
        match receiver.await {
            Ok(Ok(value)) => Ok(value.map(Bytes::from)),
            Ok(Err(error)) => Err(failed(error)),
            Err(_) => Err(failed("the store dropped the request")),
        }
    }
}

impl KvStore for PyStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(self.request(GET, key))
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.schedule(SET, key, Some(value), None)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.schedule(CLEAR, key, None, None)
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { self.request(SYNC, "").await.map(drop) })
    }
}

/// The page cache shared by the process's databases: 256 MiB of memory and
/// 1 GiB of disk, in a file under the temporary directory, or the file
/// `SQLITE_KV_CACHE_FILE` names.
static CACHE: OnceCell<PageCache> = OnceCell::const_new();

async fn page_cache() -> PyResult<&'static PageCache> {
    CACHE
        .get_or_try_init(async || {
            let named = std::env::var_os("SQLITE_KV_CACHE_FILE").map(PathBuf::from);
            let file = named.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("sqlite-kv-py-{}.cache", std::process::id()))
            });
            let cache = PageCache::open(PageCacheConfig::new(&file, 256 << 20, 1 << 30)).await?;
            // The cache keeps the file open, and nothing else uses the
            // temporary one: unlinked now, it goes away with the process
            // however it ends (where unlinking an open file is refused, it
            // stays).
            if named.is_none() {
                let _ = std::fs::remove_file(&file);
            }
            Ok::<_, io::Error>(cache)
        })
        .await
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

fn sql_error(error: Box<dyn std::error::Error + Send + Sync>) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn closed() -> PyErr {
    PyRuntimeError::new_err("the database is closed")
}

/// One session of a namespace's database.
#[pyclass(frozen)]
struct Database {
    session: Arc<Mutex<Option<Session>>>,
    /// Whether the session can still run SQL, as of the last call that ended:
    /// read without the lock, which a running call holds.
    usable: Arc<AtomicBool>,
}

/// Open the database of `namespace` over the store behind `dispatch`, which
/// runs on `event_loop`; see `sqlite_kv/__init__.py` for the store object it
/// is built from.
///
/// A `reader` reads the namespace beside its owner (see `Session::open_reader`):
/// it writes nothing to the store, and `is_current` says whether what it read
/// is still the stored state.
#[pyfunction]
#[pyo3(signature = (namespace, dispatch, event_loop, reader = false))]
fn open<'py>(
    py: Python<'py>,
    namespace: String,
    dispatch: Py<PyAny>,
    event_loop: Py<PyAny>,
    reader: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let store = Arc::new(PyStore {
        event_loop,
        dispatch,
    });
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let cache = match reader {
            true => None,
            false => Some(page_cache().await?.namespace(&namespace)),
        };
        let options = StorageOptions {
            cache,
            durability: Durability::Commit,
            ..StorageOptions::default()
        };
        let session = match reader {
            true => Session::open_reader(store, options).await,
            false => Session::open_with_options(store, options).await,
        }
        .map_err(sql_error)?;
        Ok(Database {
            session: Arc::new(Mutex::new(Some(session))),
            usable: Arc::new(AtomicBool::new(true)),
        })
    })
}

#[pymethods]
impl Database {
    /// Run SQL (one or more statements; parameters bind to a single one) and
    /// return the result rows. A statement that commits completes once its
    /// writes are stored.
    #[pyo3(signature = (sql, params=None))]
    fn query<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = params.map(|params| sql_params(&params)).transpose()?;
        let session = self.session.clone();
        let usable = self.usable.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut session = session.lock().await;
            let session = session.as_mut().ok_or_else(closed)?;
            let rows = match params {
                Some(params) => session.query_with(&sql, params).await,
                None => session.query(&sql).await,
            };
            usable.store(session.is_usable(), Ordering::Release);
            let rows = rows.map_err(sql_error)?;
            Python::attach(|py| python_rows(py, rows))
        })
    }

    /// [`Self::query`], returning `(columns, rows)`: also the column names of
    /// the statement whose rows these are.
    #[pyo3(signature = (sql, params=None))]
    fn query_columns<'py>(
        &self,
        py: Python<'py>,
        sql: String,
        params: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = params.map(|params| sql_params(&params)).transpose()?;
        let session = self.session.clone();
        let usable = self.usable.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut session = session.lock().await;
            let session = session.as_mut().ok_or_else(closed)?;
            let result = session.query_columns(&sql, params).await;
            usable.store(session.is_usable(), Ordering::Release);
            let (columns, rows) = result.map_err(sql_error)?;
            Python::attach(|py| {
                let rows = python_rows(py, rows)?;
                Ok(
                    PyTuple::new(py, [columns.into_pyobject(py)?.into_any().unbind(), rows])?
                        .unbind(),
                )
            })
        })
    }

    /// `COMMIT` the open transaction; with `effects`, they commit with it
    /// (see `Session::commit_with`).
    #[pyo3(signature = (effects=None))]
    fn commit<'py>(
        &self,
        py: Python<'py>,
        effects: Option<Vec<u8>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        let usable = self.usable.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut session = session.lock().await;
            let session = session.as_mut().ok_or_else(closed)?;
            let result = match effects {
                Some(effects) => session.commit_with(Bytes::from(effects)).await,
                None => session.execute("COMMIT").await,
            };
            usable.store(session.is_usable(), Ordering::Release);
            result.map_err(sql_error)
        })
    }

    /// Whether the session can still run SQL: an SQL error leaves it usable,
    /// a storage error ends it. A call that is running does not change the
    /// answer until it ends.
    fn is_usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    /// The effects of the last commit that set them (see `Session::effects`).
    fn effects<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = session.lock().await;
            let effects = session.as_ref().ok_or_else(closed)?.effects();
            Python::attach(|py| Ok(PyBytes::new(py, &effects).unbind()))
        })
    }

    /// Whether the store still holds the state this reader opened on, so
    /// that what it read is: if not, read again in a new session (see
    /// `Session::is_current`).
    fn is_current<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = session.lock().await;
            session
                .as_ref()
                .ok_or_else(closed)?
                .is_current()
                .await
                .map_err(sql_error)
        })
    }

    /// End the session, leaving its engine to the next one.
    fn save<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        let usable = self.usable.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = session.lock().await.take();
            usable.store(false, Ordering::Release);
            let session = session.ok_or_else(closed)?;
            session.save().await.map(drop).map_err(sql_error)
        })
    }

    /// End the session.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.session.clone();
        let usable = self.usable.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session.lock().await.take();
            usable.store(false, Ordering::Release);
            Ok(())
        })
    }
}

/// SQL values from Python: `None`, `bool` (0 or 1), `int` (64 bits),
/// `float`, `str`, and `bytes`, `bytearray` or `memoryview` (BLOB).
fn sql_params(params: &Bound<'_, PyAny>) -> PyResult<Vec<Value>> {
    if params.is_instance_of::<PyString>() || params.is_instance_of::<PyBytes>() {
        return Err(PyTypeError::new_err(
            "SQL parameters are a sequence of values",
        ));
    }
    params.try_iter()?.map(|param| sql_value(&param?)).collect()
}

fn sql_value(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    if value.is_none() {
        Ok(Value::Null)
    } else if value.is_instance_of::<PyBool>() {
        Ok(Value::from_i64(i64::from(value.extract::<bool>()?)))
    } else if value.is_instance_of::<PyInt>() {
        let number = value
            .extract::<i64>()
            .map_err(|_| PyValueError::new_err("an integer parameter exceeds 64 bits"))?;
        Ok(Value::from_i64(number))
    } else if value.is_instance_of::<PyFloat>() {
        Ok(Value::from_f64(value.extract::<f64>()?))
    } else if value.is_instance_of::<PyString>() {
        Ok(Value::build_text(value.extract::<String>()?))
    } else if value.is_instance_of::<PyBytes>() || value.is_instance_of::<PyByteArray>() {
        let bytes: Vec<u8> = value.extract()?;
        Value::from_slice(&bytes).map_err(|e| PyValueError::new_err(e.to_string()))
    } else if let Ok(bytes) = value.call_method0("tobytes") {
        // A memoryview.
        let bytes: Vec<u8> = bytes.extract()?;
        Value::from_slice(&bytes).map_err(|e| PyValueError::new_err(e.to_string()))
    } else {
        Err(PyTypeError::new_err(format!(
            "unsupported SQL value of type {}",
            value.get_type().name()?
        )))
    }
}

/// Rows as a list of lists of `None`, `int`, `float`, `str` and `bytes`.
fn python_rows(py: Python<'_>, rows: Vec<Vec<Value>>) -> PyResult<Py<PyAny>> {
    let list = PyList::empty(py);
    for row in rows {
        let values = row
            .into_iter()
            .map(|value| python_value(py, value))
            .collect::<PyResult<Vec<_>>>()?;
        list.append(PyList::new(py, values)?)?;
    }
    Ok(list.into_any().unbind())
}

fn python_value(py: Python<'_>, value: Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Numeric(Numeric::Integer(i)) => i.into_pyobject(py)?.into_any().unbind(),
        Value::Numeric(Numeric::Float(f)) => f64::from(f).into_pyobject(py)?.into_any().unbind(),
        Value::Text(text) => PyString::new(py, text.as_str()).into_any().unbind(),
        Value::Blob(blob) => PyBytes::new(py, &blob).into_any().unbind(),
    })
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(open, module)?)?;
    module.add_class::<Database>()?;
    module.add("GET", GET)?;
    module.add("SET", SET)?;
    module.add("CLEAR", CLEAR)?;
    module.add("SYNC", SYNC)?;
    Ok(())
}

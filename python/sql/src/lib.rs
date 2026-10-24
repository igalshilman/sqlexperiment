//! Python bindings: SQL over a key/value store implemented in Python.
//!
//! The host passes its asyncio event loop and one dispatcher function,
//! `dispatch(op, key, value, reply)`, which `restate_sql/__init__.py` builds from
//! a store object (`get`, `set`, `clear`, `sync`). Every request is scheduled
//! on the loop with `call_soon_threadsafe`, which runs callbacks in the order
//! they were scheduled, so a read sees the writes before it.
//!
//! `set` and `clear` queue a write; `sync` resolves once every write before it
//! is stored. The object's state keeps writes in issue order, so a crash
//! loses only a suffix of them (see [`state::kv`]): a statement that
//! commits queues its writes, then completes at one `sync`. The session itself,
//! its commits and the process-wide page cache, is [`sql::binding::Session`].
use bytes::Bytes;
use futures::future::BoxFuture;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyFloat, PyInt, PyList, PyString, PyTuple};
use sql::binding::Session;
use sql::{Numeric, Value};
use state::kv::KvStore;
use std::io;
use std::sync::Arc;
use tokio::sync::oneshot;

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

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl PyStore {
    /// Schedule `dispatch(op, key, value, reply)` on the event loop; `reply`
    /// is `None` for a write, which answers nothing.
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
        .map_err(io_error)
    }

    /// A request that answers: a read, or a sync.
    async fn request(&self, op: u32, key: &str) -> io::Result<Option<Bytes>> {
        let (sender, receiver) = oneshot::channel();
        let reply = Reply {
            sender: std::sync::Mutex::new(Some(sender)),
        };
        self.schedule(op, key, None, Some(reply))?;
        match receiver.await {
            Ok(Ok(value)) => Ok(value.map(Bytes::from)),
            Ok(Err(error)) => Err(io_error(error)),
            Err(_) => Err(io_error("the store dropped the request")),
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

fn error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// One session of a namespace's database (see [`Session`]): every SQL commit
/// is durable when its call completes.
#[pyclass(frozen)]
struct Database(Arc<Session>);

/// Open the database of `namespace` over the store behind `dispatch`, which
/// runs on `event_loop`; see `restate_sql/__init__.py` for the store object it
/// is built from, and [`Session::open`] for a `reader`.
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
        let session = Session::open(store, &namespace, reader).await;
        Ok(Database(Arc::new(session.map_err(error)?)))
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
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (_, rows) = session.query(&sql, params).await.map_err(error)?;
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
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (columns, rows) = session.query(&sql, params).await.map_err(error)?;
            Python::attach(|py| {
                let rows = python_rows(py, rows)?;
                Ok(
                    PyTuple::new(py, [columns.into_pyobject(py)?.into_any().unbind(), rows])?
                        .unbind(),
                )
            })
        })
    }

    /// See [`Session::begin`].
    fn begin<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session.begin().await.map_err(error)
        })
    }

    /// See [`Session::commit`].
    #[pyo3(signature = (effects=None))]
    fn commit<'py>(
        &self,
        py: Python<'py>,
        effects: Option<Vec<u8>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let effects = effects.map(Bytes::from);
            session.commit(effects).await.map_err(error)
        })
    }

    /// See [`Session::rollback`].
    fn rollback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session.rollback().await.map_err(error)
        })
    }

    /// See [`Session::is_usable`].
    fn is_usable(&self) -> bool {
        self.0.is_usable()
    }

    /// See [`Session::effects`].
    fn effects<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let effects = session.effects().await.map_err(error)?;
            Python::attach(|py| Ok(PyBytes::new(py, &effects).unbind()))
        })
    }

    /// See [`Session::save`].
    fn save<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session.save().await.map_err(error)
        })
    }

    /// See [`Session::close`].
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.0.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session.close().await;
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

//! The object's state as sqlite-kv's store: ordered, as Restate stores state
//! writes in the order they are made. It remembers every key it read or
//! wrote, for the invocation: each turn opens the database again, and would
//! otherwise read the same keys from Restate again, each read a round trip
//! and a journal entry.
use bytes::Bytes;
use futures::{FutureExt, future::BoxFuture};
use restate_sdk::context::RawState;
use sqlite_kv::kv::KvStore;
use std::{
    collections::HashMap,
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct ObjectStore {
    state: RawState,
    /// Each key's value as of this invocation's last read or write of it.
    known: Mutex<HashMap<String, Option<Bytes>>>,
    failed: AtomicBool,
}

impl ObjectStore {
    pub(crate) fn new(state: RawState) -> Self {
        Self {
            state,
            known: Mutex::default(),
            failed: AtomicBool::new(false),
        }
    }

    /// Whether a read or a sync failed: the turn's error is then the
    /// invocation's (retried), not the application's.
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn fail(&self, error: restate_sdk::errors::TerminalError) -> io::Error {
        self.failed.store(true, Ordering::Relaxed);
        io::Error::other(error.message().to_owned())
    }
}

impl KvStore for ObjectStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        if let Some(value) = self.known.lock().unwrap().get(key) {
            return futures::future::ready(Ok(value.clone())).boxed();
        }
        self.state
            .get(key)
            .map(|result| {
                let value = result.map_err(|e| self.fail(e))?;
                self.known
                    .lock()
                    .unwrap()
                    .insert(key.to_owned(), value.clone());
                Ok(value)
            })
            .boxed()
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        self.known
            .lock()
            .unwrap()
            .insert(key.to_owned(), Some(value.clone()));
        self.state.set(key, value);
        futures::future::ready(Ok(())).boxed()
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.known.lock().unwrap().insert(key.to_owned(), None);
        self.state.clear(key);
        futures::future::ready(Ok(())).boxed()
    }

    fn ordered(&self) -> bool {
        true
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        self.state
            .sync()
            .map(|result| result.map_err(|e| self.fail(e)))
            .boxed()
    }
}

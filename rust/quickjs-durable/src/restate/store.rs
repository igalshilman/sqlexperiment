//! The object's state as the scope's store: a [`Queued`] store, so writes
//! return at once and a `sync` waits until Restate stored them. It remembers
//! every key it read or wrote, for the attempt: the handler is the object's
//! only writer meanwhile, so a key is read from Restate at most once: a round
//! trip unless the invocation started with it (eager state), and never a
//! journal entry (protocol v8's unjournaled get).
use bytes::Bytes;
use futures::future::BoxFuture;
use restate_sdk::context::RawState;
use sqlite_kv::kv::Queued;
use std::{
    collections::HashMap,
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct StateQueue {
    state: RawState,
    /// Each key's value as of this attempt's last read or write of it.
    known: Mutex<HashMap<String, Option<Bytes>>>,
    failed: AtomicBool,
}

impl StateQueue {
    pub(crate) fn new(state: RawState) -> Self {
        Self {
            state,
            known: Mutex::default(),
            failed: AtomicBool::new(false),
        }
    }

    /// Whether Restate failed a read or a sync: the turn's error is then the
    /// attempt's (retried), not the application's.
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn fail(&self, error: restate_sdk::errors::TerminalError) -> io::Error {
        self.failed.store(true, Ordering::Relaxed);
        io::Error::other(error.message().to_owned())
    }
}

impl Queued for StateQueue {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            if let Some(value) = self.known.lock().unwrap().get(key) {
                return Ok(value.clone());
            }
            let value = self.state.get(key).await.map_err(|e| self.fail(e))?;
            self.known
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.clone());
            Ok(value)
        })
    }

    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()> {
        match &value {
            Some(value) => self.state.set(key, value.clone()),
            None => self.state.clear(key),
        }
        self.known.lock().unwrap().insert(key.to_owned(), value);
        Ok(())
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.state.sync().await.map_err(|e| self.fail(e)) })
    }
}

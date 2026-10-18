//! The object's state as a [`Store`]. It remembers every key it read or
//! wrote, for the attempt: the handler is the object's only writer meanwhile,
//! so a key is read from Restate (a round trip, and a journal entry) at most
//! once. Writes are ordered, as Restate journals them: a crash loses a suffix.
use crate::Store;
use bytes::Bytes;
use restate_sdk::context::RawState;
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct ObjectStore {
    state: RawState,
    /// Each key's value as of this attempt's last read or write of it.
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

    /// Whether a read failed: the turn's error is then the attempt's
    /// (retried), not the application's.
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Store for ObjectStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(value) = self.known.lock().unwrap().get(key) {
            return Ok(value.as_ref().map(|bytes| bytes.to_vec()));
        }
        let value = self.state.get(key).await.map_err(|error| {
            self.failed.store(true, Ordering::Relaxed);
            anyhow::anyhow!(error.message().to_owned())
        })?;
        let read = value.as_ref().map(|bytes| bytes.to_vec());
        self.known.lock().unwrap().insert(key.to_owned(), value);
        Ok(read)
    }

    async fn put(&self, key: &str, value: Vec<u8>) -> anyhow::Result<()> {
        let value = Bytes::from(value);
        self.known
            .lock()
            .unwrap()
            .insert(key.to_owned(), Some(value.clone()));
        self.state.set(key, value);
        Ok(())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.known.lock().unwrap().insert(key.to_owned(), None);
        self.state.clear(key);
        Ok(())
    }
}

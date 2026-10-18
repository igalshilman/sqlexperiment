//! Where a scope's runs and KV live: a key/value store with one writer, the
//! scope's owner, at a time. A write may be acknowledged before it is durable,
//! as long as a failure loses only a suffix of the writes, in issue order:
//! Restate state is such a store. Storage is trusted; nothing is verified.
use anyhow::Result;
use std::{collections::BTreeMap, future::Future, sync::Mutex};

pub trait Store {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>>>;
    fn put(&self, key: &str, value: Vec<u8>) -> impl Future<Output = Result<()>>;
    fn delete(&self, key: &str) -> impl Future<Output = Result<()>>;
}

/// A store in memory, for tests and benchmarks. It keeps every write in
/// order, so a test can lose any suffix of them, as a crash may.
#[derive(Default)]
pub struct MemoryStore {
    writes: Mutex<Vec<(String, Option<Vec<u8>>)>>,
}

impl MemoryStore {
    /// The store as a crash would leave it, keeping only the first `kept` writes.
    pub fn crashed(&self, kept: usize) -> Self {
        let writes = self.writes.lock().unwrap();
        Self {
            writes: Mutex::new(writes[..kept].to_vec()),
        }
    }

    /// Writes so far: puts and deletes.
    pub fn writes(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    /// Bytes put so far, keys included.
    pub fn bytes_written(&self) -> usize {
        let writes = self.writes.lock().unwrap();
        writes
            .iter()
            .map(|(key, value)| key.len() + value.as_ref().map_or(0, Vec::len))
            .sum()
    }

    /// Every key and its value, as of the last write.
    pub fn values(&self) -> BTreeMap<String, Vec<u8>> {
        let mut values = BTreeMap::new();
        for (key, value) in self.writes.lock().unwrap().iter() {
            match value {
                Some(value) => values.insert(key.clone(), value.clone()),
                None => values.remove(key),
            };
        }
        values
    }
}

impl Store for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let writes = self.writes.lock().unwrap();
        Ok(writes
            .iter()
            .rev()
            .find(|(written, _)| written == key)
            .and_then(|(_, value)| value.clone()))
    }

    async fn put(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.writes.lock().unwrap().push((key.into(), Some(value)));
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.writes.lock().unwrap().push((key.into(), None));
        Ok(())
    }
}

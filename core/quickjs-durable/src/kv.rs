use crate::model::hash;
use crate::{Operation, Outcome, Resolution, Transaction};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Reliable byte KV access. In an engine transaction, put only stages a value
/// and reads observe staged writes. The outer host controls atomic durability.
/// Both may await I/O, such as a remote store's: the engine holds no Wasm call
/// while it waits. Both take `&mut self`: a store's session may need exclusive
/// access even to read.
pub trait Kv {
    fn get(&mut self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>>>;
    fn put(&mut self, key: &str, value: &[u8]) -> impl Future<Output = Result<()>>;
}

/// Isolated transaction workspace for examples, tests and benchmarks.
/// The host clones committed state, runs the engine against this workspace,
/// then atomically publishes the whole value or discards it. This is not a
/// database or automatic transaction manager. It represents one application scope.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MemoryTransaction {
    pub values: BTreeMap<String, Vec<u8>>,
    pub state: BTreeMap<String, Value>,
    pub effects: BTreeMap<String, Operation>,
}
impl Kv for MemoryTransaction {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.values.get(key).cloned())
    }
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.values.insert(key.into(), value.into());
        Ok(())
    }
}

/// Whether `id` is one of `run`'s operation IDs: `<run>/<n>`.
fn owned_by(id: &str, run: &str) -> bool {
    id.rsplit_once('/').is_some_and(|(owner, _)| owner == run)
}

impl Transaction for MemoryTransaction {
    async fn execute(&mut self, operation: &Operation) -> Result<Resolution> {
        let value = match operation {
            Operation::KvGet { key, .. } => self.state.get(key).cloned().unwrap_or(Value::Null),
            Operation::KvSet { key, value, .. } => {
                self.state.insert(key.clone(), value.clone());
                Value::Null
            }
            Operation::KvDelete { key, .. } => {
                self.state.remove(key);
                Value::Null
            }
            _ => {
                self.effects
                    .insert(operation.id().into(), operation.clone());
                return Ok(Resolution::Pending);
            }
        };
        Ok(Resolution::Ready(Outcome::success(value)))
    }
    async fn acknowledge(
        &mut self,
        operation_id: &str,
        _outcome: &Outcome,
        _now: u64,
    ) -> Result<()> {
        // This test workspace's embedding supplies the recoverable result.
        anyhow::ensure!(
            self.effects.remove(operation_id).is_some(),
            "Missing effect"
        );
        Ok(())
    }
    async fn abandon(&mut self, run: &str) -> Result<()> {
        self.effects.retain(|id, _| !owned_by(id, run));
        Ok(())
    }
}

/// A durable file store for process-crash tests and the runnable examples: the
/// host publishes its whole transaction domain as one value. Not an engine `Kv`.
/// POSIX file implementation.
/// File fsync -> rename -> directory fsync. Hashed filenames avoid path traversal
/// and filesystem name-length limits. The host must retain the directory itself.
pub struct FileKv {
    root: PathBuf,
}
impl FileKv {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        Ok(Self {
            root: path.as_ref().to_owned(),
        })
    }
    fn path(&self, key: &str) -> PathBuf {
        self.root.join(hash(key.as_bytes()))
    }
}
impl FileKv {
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("KV read"),
        }
    }
    pub fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let (temp, mut file) = loop {
            let path = self.root.join(format!(
                ".tmp-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("KV temporary file"),
            }
        };
        let result = (|| {
            file.write_all(value)?;
            file.sync_all()?;
            fs::rename(&temp, self.path(key))?;
            File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

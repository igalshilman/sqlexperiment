//! A turn's transaction: the host seals it, and everything the run keeps
//! takes part in it.
//!
//! The seal is one atomic write, the scope's database's commit: whether the
//! turn happened is whether its seal was written. Each [`Participant`] (the
//! run's state, its memory, its tables, each extension) keeps its part in
//! one of three places around it:
//!
//! - **In the seal**: a small record ([`Sink::record`]), which the next
//!   transaction starts from.
//! - **Before it, named by it**: bytes too large for the seal go to the
//!   scope's [`ScopeBlobs`] first; the record names them. A blob no seal
//!   names is garbage, which the host collects.
//! - **After it**: what reaches the outside world (a request, a command) is
//!   an operation the seal records, made once it is written, and again on
//!   another attempt (an outbox).
//!
//! So no participant ever holds a vote it must keep across a crash: it can
//! refuse only before the seal, by failing [`Participant::prepare`]. And
//! [`Participant::committed`] and [`Participant::aborted`] are notices, which
//! a crash may skip: what a participant is, it recovers from the seal.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlite_kv::ChainUpdate;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Something a turn's transaction keeps.
#[async_trait]
pub trait Participant: Send {
    /// Before the seal: put what changed in `sink`. An error abandons the
    /// transaction (and the attempt, which starts again from the last seal).
    async fn prepare(&mut self, _sink: &mut Sink) -> Result<()> {
        Ok(())
    }

    /// The seal is written.
    fn committed(&mut self) {}

    /// The transaction is abandoned: what was prepared is not sealed.
    fn aborted(&mut self) {}

    /// The blobs the last seal's record names, or `None` while some it will
    /// name may still be in flight (written by an operation the host is
    /// making): until then, the host collects none.
    fn holds(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::new())
    }
}

/// What a participant puts in the seal.
pub struct Sink {
    record: Option<Value>,
    chain: Option<ChainUpdate>,
}

impl Sink {
    /// The record the last seal kept, which this one keeps unless replaced.
    pub fn kept(&self) -> Option<&Value> {
        self.record.as_ref()
    }

    /// Keep `record` in the seal instead.
    pub fn record(&mut self, record: Value) {
        self.record = Some(record);
    }

    /// Change the database's chain with the seal: the run's memory's.
    pub(crate) fn chain(&mut self, update: ChainUpdate) {
        self.chain = Some(update);
    }
}

/// The seal: each participant's record, by name. What the scope's
/// database's commit holds as its effects, with its SQL and its chain.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Seal {
    /// Seals written in the scope, this one included.
    pub seq: u64,
    pub parts: BTreeMap<String, Value>,
}

/// The next seal, as its participants prepare it.
pub(crate) struct Sealing {
    seal: Seal,
    chain: Option<ChainUpdate>,
}

impl Sealing {
    /// After `previous`, whose records it keeps unless replaced.
    pub fn after(previous: &Seal) -> Self {
        let seal = Seal {
            seq: previous.seq + 1,
            parts: previous.parts.clone(),
        };
        Self { seal, chain: None }
    }

    /// `participant`'s part, as `name`.
    pub async fn prepare(&mut self, name: &str, participant: &mut dyn Participant) -> Result<()> {
        let mut sink = Sink {
            record: self.seal.parts.remove(name),
            chain: None,
        };
        participant.prepare(&mut sink).await?;
        self.seal
            .parts
            .extend(sink.record.map(|record| (name.to_owned(), record)));
        if let Some(chain) = sink.chain {
            ensure!(self.chain.is_none(), "Two participants change the chain");
            self.chain = Some(chain);
        }
        Ok(())
    }

    /// The seal, and the chain's change.
    pub fn finish(self) -> (Seal, Option<ChainUpdate>) {
        (self.seal, self.chain)
    }
}

/// Where a deployment keeps blobs, per scope, by name.
#[async_trait]
pub trait Blobs: Send + Sync + 'static {
    async fn put(&self, scope: &str, name: &str, bytes: &[u8]) -> Result<()>;
    async fn get(&self, scope: &str, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self, scope: &str) -> Result<Vec<String>>;
    async fn delete(&self, scope: &str, name: &str) -> Result<()>;
}

/// A scope's blobs, as participants see them: they write and read; the
/// host alone deletes. A blob's name is its content's hash.
#[derive(Clone)]
pub struct ScopeBlobs {
    blobs: Option<Arc<dyn Blobs>>,
    scope: String,
}

impl ScopeBlobs {
    pub(crate) fn new(blobs: Option<Arc<dyn Blobs>>, scope: &str) -> Self {
        Self {
            blobs,
            scope: scope.to_owned(),
        }
    }

    fn store(&self) -> Result<&dyn Blobs> {
        (self.blobs.as_deref())
            .context("This deployment keeps no blobs: see Extensions::with_blobs")
    }

    /// Write `bytes`: its name.
    pub async fn put(&self, bytes: &[u8]) -> Result<String> {
        let name = hash(bytes);
        self.store()?.put(&self.scope, &name, bytes).await?;
        Ok(name)
    }

    pub async fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.store()?.get(&self.scope, name).await
    }

    /// Delete the blobs that none of `holding` names: after a seal, when
    /// none has any in flight.
    pub(crate) async fn collect(&self, holding: &[Option<BTreeSet<String>>]) -> Result<usize> {
        let Some(blobs) = &self.blobs else {
            return Ok(0);
        };
        let mut kept = BTreeSet::new();
        for holds in holding {
            match holds {
                Some(names) => kept.extend(names.iter().map(String::as_str)),
                None => return Ok(0),
            }
        }
        let mut deleted = 0;
        for name in blobs.list(&self.scope).await? {
            if !kept.contains(name.as_str()) {
                blobs.delete(&self.scope, &name).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

/// Blobs in a directory: `<root>/<the scope's hash>/<name>`.
pub struct DirBlobs {
    root: PathBuf,
}

impl DirBlobs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// In the directory FUNCTIONS_BLOBS, or else in `functions-blobs` under
    /// the system's temporary directory.
    pub fn from_env() -> Self {
        let root = std::env::var_os("FUNCTIONS_BLOBS")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("functions-blobs"));
        Self::new(root)
    }

    fn dir(&self, scope: &str) -> PathBuf {
        self.root.join(&hash(scope.as_bytes())[..32])
    }
}

#[async_trait]
impl Blobs for DirBlobs {
    async fn put(&self, scope: &str, name: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.dir(scope);
        let path = dir.join(name);
        if tokio::fs::try_exists(&path).await? {
            return Ok(());
        }
        tokio::fs::create_dir_all(&dir).await?;
        // Whole or not at all: written aside, then renamed into place.
        let partial = dir.join(format!("{name}.{}.partial", random()));
        tokio::fs::write(&partial, bytes).await?;
        tokio::fs::rename(&partial, &path).await?;
        Ok(())
    }

    async fn get(&self, scope: &str, name: &str) -> Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.dir(scope).join(name)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut entries = match tokio::fs::read_dir(self.dir(scope)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            names.extend(entry.file_name().to_str().map(str::to_owned));
        }
        Ok(names)
    }

    async fn delete(&self, scope: &str, name: &str) -> Result<()> {
        match tokio::fs::remove_file(self.dir(scope).join(name)).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/// Blobs in memory, for tests.
#[derive(Default)]
pub struct MemoryBlobs(Mutex<BTreeMap<(String, String), Vec<u8>>>);

#[async_trait]
impl Blobs for MemoryBlobs {
    async fn put(&self, scope: &str, name: &str, bytes: &[u8]) -> Result<()> {
        let mut blobs = self.0.lock().unwrap();
        blobs.insert((scope.to_owned(), name.to_owned()), bytes.to_vec());
        Ok(())
    }

    async fn get(&self, scope: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let blobs = self.0.lock().unwrap();
        Ok(blobs.get(&(scope.to_owned(), name.to_owned())).cloned())
    }

    async fn list(&self, scope: &str) -> Result<Vec<String>> {
        let blobs = self.0.lock().unwrap();
        let names = blobs.keys().filter(|(owner, _)| owner == scope);
        Ok(names.map(|(_, name)| name.clone()).collect())
    }

    async fn delete(&self, scope: &str, name: &str) -> Result<()> {
        let mut blobs = self.0.lock().unwrap();
        blobs.remove(&(scope.to_owned(), name.to_owned()));
        Ok(())
    }
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn random() -> u64 {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).expect("randomness");
    u64::from_le_bytes(bytes)
}

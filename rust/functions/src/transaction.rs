//! A turn's transaction: the host seals it, and everything the run keeps
//! takes part in it.
//!
//! The seal is one [`Wal`] of the scope's [`State`], which every participant
//! writes into and the host flushes in one put: whether the turn happened is
//! whether its seal was flushed. Each [`Participant`] (the run's progress,
//! its memory, its tables, each extension) keeps its part in one of three
//! places around it:
//!
//! - **In the seal**: a [`Record`], a named whole value the commit record
//!   holds, which the next transaction starts from (or a file of the scope's
//!   [`State`]).
//! - **Before it, named by it**: bytes too large for a record go to the
//!   scope's [`ScopeBlobs`] first; the record names them. A blob no record
//!   names is garbage, which the host collects.
//! - **After it**: what reaches the outside world (a request, a command) is
//!   an operation the seal records, made once it is flushed, and again on
//!   another attempt (an outbox).
//!
//! So no participant ever holds a vote it must keep across a crash: it can
//! refuse only before the flush, by failing [`Participant::prepare`]; the
//! WAL then drops, and its writes with it. And [`Participant::committed`]
//! and [`Participant::aborted`] are notices, which a crash may skip: what a
//! participant is, it recovers from the seal.
use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
pub use state::{Record, State, Wal};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

/// Something a turn's transaction keeps.
#[async_trait]
pub trait Participant: Send {
    /// Before the seal: set the records that changed, and write the pages,
    /// in `wal`. An error abandons the transaction (and the attempt, which
    /// starts again from the last seal).
    async fn prepare(&mut self, _wal: &mut Wal) -> Result<()> {
        Ok(())
    }

    /// The seal is flushed.
    fn committed(&mut self) {}

    /// The transaction is abandoned: what was prepared is not sealed.
    fn aborted(&mut self) {}

    /// The blobs the last seal's records name, or `None` while some it will
    /// name may still be in flight (written by an operation the host is
    /// making): until then, the host collects none.
    fn holds(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::new())
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
            // A `.partial` file is a put in flight, not a blob yet.
            let name = entry.file_name().to_str().map(str::to_owned);
            names.extend(name.filter(|name| !name.ends_with(".partial")));
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

/// A blob's name: its content's SHA-256, in hex.
pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A random number, to name temporary files.
pub(crate) fn random() -> u64 {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).expect("randomness");
    u64::from_le_bytes(bytes)
}

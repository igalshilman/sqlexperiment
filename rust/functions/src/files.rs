//! `fs`: a scope's files, checkpointed with its commits ([`Files`], the
//! extension behind `ctx.fs`). A toy, to try the extension contract with
//! state of its own: text files, kept whole.
//!
//! - **In the turn**, `ctx.fs.read`, `write`, `remove` and `list` see the
//!   files as the last commit left them, with the turn's changes over them.
//! - **Before the commit**, the turn's changes become a checkpoint: each
//!   file's content is a blob named by its hash, written first, and the
//!   manifest (each path's blob) is the record `fs/manifest`, in the seal.
//!   A crash before the seal leaves blobs no seal names, which the host
//!   collects.
//! - **`ctx.fs.exec(command)`** runs a command after the commit, in a
//!   directory holding the committed checkpoint. The files it changed come
//!   back as blobs, and join the files in the turn its outcome arrives; the
//!   program sees their paths. A crash meanwhile runs it again, on the same
//!   checkpoint: what it did to the files cannot happen twice. While it runs,
//!   the files cannot change.
//!
//! The blobs are the scope's [`ScopeBlobs`] (a directory here; object
//! storage in production): none of the files' contents are in the program's
//! heap, the seal, or Restate.
use crate::{
    GuestError, Operation, Outcome,
    extension::{Attached, Extension, HostCall, Making, Tables},
    transaction::{Journal, Participant, Record, ScopeBlobs, hash, random},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

/// How long a command may run, when it does not say.
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// The extension: each scope's files, checkpointed in its blobs.
#[derive(Clone, Default)]
pub struct Files;

impl Files {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extension for Files {
    /// The scope's files, as its records `manifest` and `running` keep them.
    async fn attach(&self, blobs: ScopeBlobs, journal: Journal) -> Result<Box<dyn Attached>> {
        let (manifest_record, running_record) =
            (journal.record("manifest"), journal.record("running"));
        let manifest = match manifest_record.get() {
            bytes if bytes.is_empty() => BTreeMap::new(),
            bytes => serde_json::from_slice(&bytes)?,
        };
        let running = match running_record.get() {
            bytes if bytes.is_empty() => None,
            bytes => Some(String::from_utf8(bytes.to_vec())?),
        };
        Ok(Box::new(Checkpointed {
            blobs,
            manifest_record,
            running_record,
            manifest,
            changes: BTreeMap::new(),
            saved_running: running.clone(),
            running,
        }))
    }
}

/// A turn's change to a file.
enum Change {
    Text(String),
    /// Its new content, a blob already written (by a command).
    Blob(String),
    Removed,
}

/// The scope's files in a run.
struct Checkpointed {
    blobs: ScopeBlobs,
    /// Its records: the manifest, and the command running.
    manifest_record: Record,
    running_record: Record,
    /// The last commit's files: each path's blob.
    manifest: BTreeMap<String, String>,
    /// What the turn changed, not yet in a checkpoint.
    changes: BTreeMap<String, Change>,
    /// The command running, if any: its operation. The files cannot change
    /// meanwhile.
    running: Option<String>,
    saved_running: Option<String>,
}

impl Checkpointed {
    /// The file at `path`, as the turn sees it: its content, if any.
    async fn read(&self, path: &str) -> Result<Option<String>> {
        let blob = match self.changes.get(path) {
            Some(Change::Text(text)) => return Ok(Some(text.clone())),
            Some(Change::Removed) => return Ok(None),
            Some(Change::Blob(blob)) => blob,
            None => match self.manifest.get(path) {
                Some(blob) => blob,
                None => return Ok(None),
            },
        };
        let bytes = (self.blobs.get(blob).await?)
            .with_context(|| format!("Missing blob {blob} of {path}"))?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn exists(&self, path: &str) -> bool {
        match self.changes.get(path) {
            Some(Change::Removed) => false,
            Some(_) => true,
            None => self.manifest.contains_key(path),
        }
    }

    fn unchanging(&self) -> Result<(), GuestError> {
        match &self.running {
            Some(_) => Err(GuestError::new(
                "Error",
                "The files cannot change while a command runs",
            )),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl Attached for Checkpointed {
    /// `fs.read`, `fs.write`, `fs.remove` and `fs.list`.
    async fn call(
        &mut self,
        _tables: &mut Tables<'_>,
        op: &str,
        input: Value,
    ) -> Result<Value, GuestError> {
        let failed = |error: anyhow::Error| GuestError::new("Error", format!("{error:#}"));
        match op {
            "fs.read" => {
                let path = path(&input)?;
                Ok(self
                    .read(&path)
                    .await
                    .map_err(failed)?
                    .map_or(Value::Null, Value::String))
            }
            "fs.write" => {
                let path = path(&input)?;
                let Some(content) = input["content"].as_str() else {
                    return Err(type_error("A file's content is a string"));
                };
                self.unchanging()?;
                self.changes.insert(path, Change::Text(content.to_owned()));
                Ok(Value::Null)
            }
            "fs.remove" => {
                let path = path(&input)?;
                self.unchanging()?;
                let existed = self.exists(&path);
                if existed {
                    self.changes.insert(path, Change::Removed);
                }
                Ok(existed.into())
            }
            "fs.list" => {
                let prefix = input["prefix"].as_str().unwrap_or_default();
                let paths: BTreeSet<&String> = (self.manifest.keys())
                    .chain(self.changes.keys())
                    .filter(|path| path.starts_with(prefix) && self.exists(path))
                    .collect();
                Ok(json!(paths))
            }
            _ => Err(type_error(format!("No call {op}"))),
        }
    }

    /// A command, `fs.exec`: one at a time, and the files are fixed until it
    /// is done.
    fn issued(&mut self, operation: &Operation) -> Result<(), GuestError> {
        let Operation::Call { id, name, .. } = operation else {
            return Ok(());
        };
        if name != "fs.exec" {
            return Err(type_error(format!("No operation {name}")));
        }
        if self.running.is_some() {
            return Err(GuestError::new("Error", "A command is already running"));
        }
        self.running = Some(id.clone());
        Ok(())
    }

    /// Run the command on the committed checkpoint.
    async fn operation(
        &mut self,
        call: HostCall,
        _tables: &mut Tables<'_>,
    ) -> Result<Making, GuestError> {
        let Some(command) = call.input["command"].as_str().map(str::to_owned) else {
            return Err(type_error("A command is a string"));
        };
        let timeout = match call.input["timeout"].as_f64() {
            Some(seconds) if seconds > 0.0 => Duration::from_secs_f64(seconds).min(MAX_TIMEOUT),
            _ => TIMEOUT,
        };
        if !self.changes.is_empty() {
            return Err(GuestError::new(
                "Error",
                "The files changed after the commit",
            ));
        }
        let (blobs, manifest) = (self.blobs.clone(), self.manifest.clone());
        Ok(Box::pin(async move {
            exec(&blobs, &manifest, &command, timeout)
                .await
                .map_err(|error| GuestError::new("Error", format!("{error:#}")))
        }))
    }

    /// A command still running never delivers: the files may change again.
    fn ended(&mut self) {
        self.running = None;
    }

    /// The command's changes join the files; the program sees their paths.
    async fn delivered(
        &mut self,
        _tables: &mut Tables<'_>,
        operation: &Operation,
        outcome: &mut Outcome,
    ) -> Result<()> {
        if self.running.as_deref() != Some(operation.id()) {
            return Ok(());
        }
        self.running = None;
        let Outcome::Success { value, .. } = outcome else {
            return Ok(());
        };
        let mut changed = Vec::new();
        if let Some(Value::Object(changes)) =
            value.as_object_mut().and_then(|v| v.remove("changes"))
        {
            for (path, blob) in changes {
                let change = match blob {
                    Value::String(blob) => Change::Blob(blob),
                    _ => Change::Removed,
                };
                self.changes.insert(path.clone(), change);
                changed.push(path);
            }
        }
        value["changed"] = json!(changed);
        Ok(())
    }
}

#[async_trait]
impl Participant for Checkpointed {
    /// The turn's changes, as a checkpoint: its blobs written, and its
    /// manifest in the seal.
    async fn prepare(&mut self) -> Result<()> {
        if self.changes.is_empty() && self.running == self.saved_running {
            return Ok(());
        }
        let mut manifest = self.manifest.clone();
        for (path, change) in std::mem::take(&mut self.changes) {
            match change {
                Change::Text(text) => {
                    manifest.insert(path, self.blobs.put(text.as_bytes()).await?);
                }
                Change::Blob(blob) => {
                    manifest.insert(path, blob);
                }
                Change::Removed => {
                    manifest.remove(&path);
                }
            }
        }
        if manifest != self.manifest {
            self.manifest_record
                .set(serde_json::to_vec(&manifest)?.into());
            self.manifest = manifest;
        }
        if self.running != self.saved_running {
            let running = self.running.clone().unwrap_or_default();
            self.running_record.set(running.into());
            self.saved_running = self.running.clone();
        }
        Ok(())
    }

    /// The files' blobs; none while a command runs: the blobs it writes are
    /// not named yet.
    fn holds(&self) -> Option<BTreeSet<String>> {
        if self.saved_running.is_some() {
            return None;
        }
        Some(self.manifest.values().cloned().collect())
    }
}

/// Run `command` in a directory holding `manifest`'s files: its exit code,
/// its output, and the files it changed, each as a blob it wrote (or null,
/// if it removed it).
async fn exec(
    blobs: &ScopeBlobs,
    manifest: &BTreeMap<String, String>,
    command: &str,
    timeout: Duration,
) -> Result<Value> {
    let dir = std::env::temp_dir().join(format!("functions-exec-{}", random()));
    let ran = async {
        for (path, blob) in manifest {
            let bytes = (blobs.get(blob).await?).with_context(|| format!("Missing blob {blob}"))?;
            let file = dir.join(path);
            tokio::fs::create_dir_all(file.parent().context("A file has a directory")?).await?;
            tokio::fs::write(&file, bytes).await?;
        }
        tokio::fs::create_dir_all(&dir).await?;
        let (exit, output) = crate::workspace::run_command(&dir, command, timeout).await?;
        let mut changes = serde_json::Map::new();
        let mut seen = BTreeSet::new();
        for (path, bytes) in files_in(&dir).await? {
            if manifest.get(&path) != Some(&hash(&bytes)) {
                let blob = blobs.put(&bytes).await?;
                changes.insert(path.clone(), blob.into());
            }
            seen.insert(path);
        }
        for path in manifest.keys().filter(|path| !seen.contains(*path)) {
            changes.insert(path.clone(), Value::Null);
        }
        Ok(json!({"exit": exit, "output": output, "changes": changes}))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    ran
}

/// Every regular file under `dir`: its path from `dir`, and its bytes.
async fn files_in(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        let mut entries = tokio::fs::read_dir(&at).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                let relative = entry.path().strip_prefix(dir)?.to_path_buf();
                let Some(path) = relative.to_str() else {
                    bail!("A path that is not UTF-8: {relative:?}");
                };
                files.push((
                    path.replace('\\', "/"),
                    tokio::fs::read(entry.path()).await?,
                ));
            }
        }
    }
    Ok(files)
}

/// The input's `path`, normalized: relative, without `.` or `..`.
fn path(input: &Value) -> Result<String, GuestError> {
    let Some(path) = input["path"].as_str() else {
        return Err(type_error("A path is a string"));
    };
    let parts: Vec<&str> = (path.split('/'))
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") || path.starts_with('/') || path.len() > 1024 {
        return Err(type_error(format!(
            "Invalid path {path:?}: relative, without .."
        )));
    }
    Ok(parts.join("/"))
}

fn type_error(message: impl Into<String>) -> GuestError {
    GuestError::new("TypeError", message)
}

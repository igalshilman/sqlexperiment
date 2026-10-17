//! The SQLite host's tables in one namespace of a key/value store, through
//! sqlite-kv: the storage a Restate virtual object gives each key. One turn is
//! one sqlite-kv session; its changes stay private until [`NamespaceTransaction::commit`]
//! saves them, as one log transaction (usually a single put).
//!
//! The namespace is one application KV scope: every run in it shares `state`.
//! Its owner (the object's exclusive handler) runs one turn at a time.
//!
//! Unlike [`crate::sqlite`], this host keeps no results: the embedding's inbox
//! (a Restate invocation of the object, retried until its turn commits) holds a
//! completion until the engine consumes it. `effects` is the outbox: the
//! requests the host must dispatch once the turn that issued them commits, and
//! again after a crash, under their stable IDs.
use crate::{Kv, Operation, Outcome, Resolution, Transaction};
use anyhow::{Context, Result, ensure};
use serde_json::Value as Json;
use sqlite_kv::{Session, StorageOptions, Value, kv::KvStore};
use std::sync::Arc;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS checkpoints (key TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, operation BLOB NOT NULL);";

/// One turn of one run, on a namespace this process owns. Dropping it discards
/// every staged change.
pub struct NamespaceTransaction {
    db: Session,
    run: String,
}

fn sql(error: Box<dyn std::error::Error + Send + Sync>) -> anyhow::Error {
    anyhow::anyhow!(error)
}

fn blob(bytes: &[u8]) -> Result<Value> {
    Value::from_slice(bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

fn text(value: &str) -> Value {
    Value::build_text(value.to_owned())
}

impl NamespaceTransaction {
    /// Open the namespace for a turn of `run`. The caller must own it: no other
    /// session may write it until this one is committed or dropped.
    pub async fn begin(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
        run: &str,
    ) -> Result<Self> {
        let mut db = Session::open_with_options(store, options)
            .await
            .map_err(sql)?;
        db.execute(SCHEMA).await.map_err(sql)?;
        Ok(Self {
            db,
            run: run.into(),
        })
    }

    /// Make the turn durable: the checkpoint, the KV changes, the new effects
    /// and the consumed one, together. `Ok` means committed; an error leaves
    /// the outcome unknown until the namespace is opened again.
    pub async fn commit(self) -> Result<()> {
        self.db.save().await.map_err(sql)?;
        Ok(())
    }

    /// The run's requests waiting for a result, staged or committed, in ID
    /// order: dispatch each under its ID after the commit, and again on recovery.
    pub async fn effects(&mut self) -> Result<Vec<Operation>> {
        let rows = self
            .db
            .query_with(
                "SELECT operation FROM effects WHERE run = ? ORDER BY id",
                [text(&self.run)],
            )
            .await
            .map_err(sql)?;
        rows.iter()
            .map(|row| match row.first() {
                Some(Value::Blob(bytes)) => Ok(serde_json::from_slice(bytes)?),
                _ => anyhow::bail!("Invalid effect row"),
            })
            .collect()
    }

    async fn value(&mut self, query: &str, key: &str) -> Result<Option<Vec<u8>>> {
        let rows = self.db.query_with(query, [text(key)]).await.map_err(sql)?;
        match rows.first().and_then(|row| row.first()) {
            None => Ok(None),
            Some(Value::Blob(bytes)) => Ok(Some(bytes.to_vec())),
            Some(_) => anyhow::bail!("Invalid stored value"),
        }
    }

    fn check_operation(&self, operation: &Operation) -> Result<()> {
        ensure!(
            operation.id().rsplit_once('/').map(|p| p.0) == Some(self.run.as_str()),
            "Transaction belongs to a different run"
        );
        Ok(())
    }
}

impl Kv for NamespaceTransaction {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.value("SELECT value FROM checkpoints WHERE key = ?", key)
            .await
    }

    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.db
            .execute_with(
                "INSERT OR REPLACE INTO checkpoints (key, value) VALUES (?, ?)",
                [text(key), blob(value)?],
            )
            .await
            .map_err(sql)
    }
}

impl Transaction for NamespaceTransaction {
    fn check_run(&self, run: &str) -> Result<()> {
        ensure!(run == self.run, "Transaction belongs to a different run");
        Ok(())
    }

    async fn execute(&mut self, operation: &Operation) -> Result<Resolution> {
        self.check_operation(operation)?;
        let value = match operation {
            Operation::KvGet { key, .. } => {
                match self
                    .value("SELECT value FROM state WHERE key = ?", key)
                    .await?
                {
                    Some(bytes) => serde_json::from_slice(&bytes)?,
                    None => Json::Null,
                }
            }
            Operation::KvSet { key, value, .. } => {
                let bytes = serde_json::to_vec(value)?;
                self.db
                    .execute_with(
                        "INSERT OR REPLACE INTO state (key, value) VALUES (?, ?)",
                        [text(key), blob(&bytes)?],
                    )
                    .await
                    .map_err(sql)?;
                Json::Null
            }
            Operation::KvDelete { key, .. } => {
                self.db
                    .execute_with("DELETE FROM state WHERE key = ?", [text(key)])
                    .await
                    .map_err(sql)?;
                Json::Null
            }
            _ => {
                let bytes = serde_json::to_vec(operation)?;
                self.db
                    .execute_with(
                        "INSERT INTO effects (id, run, operation) VALUES (?, ?, ?)",
                        [text(operation.id()), text(&self.run), blob(&bytes)?],
                    )
                    .await
                    .map_err(sql)?;
                return Ok(Resolution::Pending);
            }
        };
        Ok(Resolution::Ready(Outcome::success(value)))
    }

    /// The inbox, not this table, holds the result: consuming it removes the
    /// request from the outbox, in the same commit as the next checkpoint.
    async fn acknowledge(&mut self, id: &str, _outcome: &Outcome, _now: u64) -> Result<()> {
        let found = self
            .db
            .query_with(
                "SELECT 1 FROM effects WHERE id = ? AND run = ?",
                [text(id), text(&self.run)],
            )
            .await
            .map_err(sql)?;
        found.first().context("No such effect in this run")?;
        self.db
            .execute_with("DELETE FROM effects WHERE id = ?", [text(id)])
            .await
            .map_err(sql)
    }
}

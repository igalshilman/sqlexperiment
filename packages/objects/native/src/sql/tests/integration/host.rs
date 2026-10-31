//! A host of the database, as the tests use it: a session over a state of
//! its own, every SQL commit durable when its call returns, as the Node.js
//! addon's.
use crate::sql::{Database, SqlOptions};
pub use crate::state::simulation::Setup as StateSetup;
use crate::state::{FlushReport, Result, StateOptions, kv::KvStore};
use bytes::Bytes;
use std::{ops::Deref, sync::Arc};

impl StateSetup {
    /// These options, changed by `change`.
    pub fn with(self, change: impl FnOnce(StateOptions) -> StateOptions) -> Self {
        Self {
            options: change(self.options),
            ..self
        }
    }
}

/// A session of a database on a state of its own.
pub struct Db(Database);

impl Deref for Db {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.0
    }
}

impl Db {
    pub async fn open(store: Arc<dyn KvStore>, setup: impl Into<StateSetup>) -> Result<Self> {
        Self::open_with(store, setup, SqlOptions::default()).await
    }

    /// [`Self::open`] with explicit SQL limits.
    pub async fn open_with(
        store: Arc<dyn KvStore>,
        setup: impl Into<StateSetup>,
        limits: SqlOptions,
    ) -> Result<Self> {
        let state = setup.into().open(store).await?;
        Ok(Self(Database::with_options(state, limits).await?))
    }

    /// End the session, leaving its engine.
    pub async fn save(self) -> Result<()> {
        self.0.save().await
    }

    /// Checkpoint ([`Database::checkpoint`]), then end the session: the
    /// database's pages are read from the checkpoint's pack after it. The
    /// checkpoint's report.
    pub async fn save_checkpointed(self) -> Result<FlushReport> {
        let report = self.0.checkpoint().await?;
        self.0.save().await?;
        Ok(report)
    }

    /// `COMMIT` the open transaction with `effects`, in one flush: what the
    /// addon's `commit(effects)` does.
    pub async fn commit_with(&self, effects: Bytes) -> Result<()> {
        self.0.commit(Some(effects)).await
    }

    /// The effects, as last flushed.
    pub fn effects(&self) -> Bytes {
        self.0.state().effects()
    }

    /// The state's version: the flushes ever made, as this session holds it.
    pub fn version(&self) -> u64 {
        self.0.state().version()
    }
}

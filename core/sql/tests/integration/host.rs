//! A host of the journal, as the tests use it: SQL on a journal of its own,
//! committed when the host saves ([`Durability::Save`]) or after each SQL
//! transaction ([`Durability::Commit`]), as the Node.js addon and the Python
//! extension do.
use bytes::Bytes;
use journal::{Journal, Result, SaveReport, StorageOptions, kv::KvStore};
use sql::{Sql, SqlOptions, Value};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

/// When the host commits the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// At [`Db::save`]: the session's SQL as one commit.
    Save,
    /// After each statement that leaves no SQL transaction open: every SQL
    /// commit durable when its statement returns.
    Commit,
}

/// How to open a [`Db`]: storage options, and when to commit.
pub struct Setup(pub StorageOptions, pub Durability);

impl Setup {
    /// These options, changed by `change`.
    pub fn with(self, change: impl FnOnce(StorageOptions) -> StorageOptions) -> Self {
        Self(change(self.0), self.1)
    }

    /// These options, committed as `durability` says.
    pub fn mode(self, durability: Durability) -> Self {
        Self(self.0, durability)
    }
}

impl From<StorageOptions> for Setup {
    fn from(options: StorageOptions) -> Self {
        Self(options, Durability::Save)
    }
}

impl From<(StorageOptions, Durability)> for Setup {
    fn from((options, durability): (StorageOptions, Durability)) -> Self {
        Self(options, durability)
    }
}

/// SQL on a journal of its own.
pub struct Db {
    sql: Sql,
    durability: Durability,
}

impl Deref for Db {
    type Target = Sql;
    fn deref(&self) -> &Sql {
        &self.sql
    }
}

impl DerefMut for Db {
    fn deref_mut(&mut self) -> &mut Sql {
        &mut self.sql
    }
}

type Rows = Vec<Vec<Value>>;

impl Db {
    pub async fn open(store: Arc<dyn KvStore>, setup: impl Into<Setup>) -> Result<Self> {
        Self::open_with(store, setup, SqlOptions::default()).await
    }

    /// [`Self::open`] with explicit SQL limits.
    pub async fn open_with(
        store: Arc<dyn KvStore>,
        setup: impl Into<Setup>,
        limits: SqlOptions,
    ) -> Result<Self> {
        let Setup(options, durability) = setup.into();
        let journal = Journal::open_with_options(store, options).await?;
        Ok(Self {
            sql: Sql::open_with_options(&journal, limits).await?,
            durability,
        })
    }

    /// Flush and commit, and end the session (leaving its engine).
    pub async fn save(mut self) -> Result<SaveReport> {
        self.sql.flush().await?;
        self.sql.journal().commit().await
    }

    /// With [`Durability::Commit`], commit what the call committed in SQL.
    async fn settle<T>(&mut self, result: Result<T>) -> Result<T> {
        if self.durability == Durability::Commit
            && self.sql.is_usable()
            && !self.sql.in_transaction()
        {
            self.sql.flush().await?;
            self.sql.journal().commit().await?;
        }
        result
    }

    pub async fn query(&mut self, sql: &str) -> Result<Rows> {
        let result = self.sql.query(sql).await;
        self.settle(result).await
    }

    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        self.query(sql).await.map(drop)
    }

    pub async fn query_with(
        &mut self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<Rows> {
        let result = self.sql.query_with(sql, params).await;
        self.settle(result).await
    }

    pub async fn execute_with(
        &mut self,
        sql: &str,
        params: impl IntoIterator<Item = Value>,
    ) -> Result<()> {
        self.query_with(sql, params).await.map(drop)
    }

    /// `COMMIT` the open transaction with `effects` as the record `effects`,
    /// and commit: what the addon's `commit(effects)` does.
    pub async fn commit_with(&mut self, effects: Bytes) -> Result<()> {
        // After the flush: a `COMMIT` that fails takes nothing from the journal.
        self.sql.flush().await?;
        self.sql.journal().record("effects").set(effects);
        self.sql.journal().commit().await.map(drop)
    }

    /// The record `effects`.
    pub fn effects(&self) -> Bytes {
        self.sql.journal().record("effects").get()
    }

    pub async fn transaction<T>(
        &mut self,
        body: impl AsyncFnOnce(&mut Sql) -> Result<T>,
    ) -> Result<T> {
        let result = self.sql.transaction(body).await;
        self.settle(result).await
    }
}

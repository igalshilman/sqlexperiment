//! A host of the state, as the tests use it: SQL on a state of its own, and
//! the turn's mutation, which the host sets records and writes files in. It
//! flushes when the host saves ([`Durability::Save`]) or after each SQL
//! transaction ([`Durability::Commit`]), as the Node.js addon and the Python
//! extension do.
use bytes::Bytes;
use sql::{Sql, SqlOptions, Value};
pub use state::simulation::Setup as StateSetup;
use state::{FlushReport, Mutation, Result, StateOptions, kv::KvStore};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

/// When the host flushes the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// At [`Db::save`]: the session's SQL as one flush.
    Save,
    /// After each statement that leaves no SQL transaction open: every SQL
    /// commit durable when its statement returns.
    Commit,
}

/// How to open a [`Db`]: its state's options and cache, and when to flush.
pub struct Setup(pub StateSetup, pub Durability);

impl Setup {
    /// These options, changed by `change`.
    pub fn with(self, change: impl FnOnce(StateOptions) -> StateOptions) -> Self {
        let Self(StateSetup { options, cache }, durability) = self;
        let options = change(options);
        Self(StateSetup { options, cache }, durability)
    }

    /// These options, flushed as `durability` says.
    pub fn mode(self, durability: Durability) -> Self {
        Self(self.0, durability)
    }
}

impl From<StateSetup> for Setup {
    fn from(state: StateSetup) -> Self {
        Self(state, Durability::Save)
    }
}

impl From<StateOptions> for Setup {
    fn from(options: StateOptions) -> Self {
        StateSetup::from(options).into()
    }
}

impl From<(StateSetup, Durability)> for Setup {
    fn from((state, durability): (StateSetup, Durability)) -> Self {
        Self(state, durability)
    }
}

impl From<(StateOptions, Durability)> for Setup {
    fn from((options, durability): (StateOptions, Durability)) -> Self {
        Self(options.into(), durability)
    }
}

/// SQL on a state of its own, and the mutation of its turn.
pub struct Db {
    sql: Sql,
    /// The turn's mutation: what the host sets beside the SQL, flushed with it.
    mutation: Mutation,
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
        let Setup(setup, durability) = setup.into();
        let state = setup.open(store).await?;
        Ok(Self {
            sql: Sql::open_with_options(&state, limits).await?,
            mutation: state.mutation(),
            durability,
        })
    }

    /// The turn's mutation, to set records and write files in: [`Self::flush`]
    /// makes them durable with the SQL.
    pub fn mutation(&mut self) -> &mut Mutation {
        &mut self.mutation
    }

    /// Flush the SQL into the turn's mutation, and the mutation: what the SQL committed
    /// and what the host set, durable together. The next turn starts a new
    /// mutation; one whose SQL flush fails keeps this one.
    pub async fn flush(&mut self) -> Result<FlushReport> {
        self.sql.flush(&mut self.mutation).await?;
        let mutation = std::mem::replace(&mut self.mutation, self.sql.state().mutation());
        mutation.flush().await
    }

    /// Flush, and end the session (leaving its engine).
    pub async fn save(mut self) -> Result<FlushReport> {
        self.flush().await
    }

    /// With [`Durability::Commit`], flush what the call committed in SQL.
    async fn settle<T>(&mut self, result: Result<T>) -> Result<T> {
        if self.durability == Durability::Commit
            && self.sql.is_usable()
            && !self.sql.in_transaction()
        {
            self.flush().await?;
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
    /// in one flush: what the addon's `commit(effects)` does.
    pub async fn commit_with(&mut self, effects: Bytes) -> Result<()> {
        // After the SQL flush: a `COMMIT` that fails sets no effects.
        self.sql.flush(&mut self.mutation).await?;
        let record = self.sql.state().record("effects");
        self.mutation.set(&record, effects)?;
        self.flush().await.map(drop)
    }

    /// The record `effects`, as last flushed.
    pub fn effects(&self) -> Bytes {
        self.sql.state().record("effects").get()
    }

    pub async fn transaction<T>(
        &mut self,
        body: impl AsyncFnOnce(&mut Sql) -> Result<T>,
    ) -> Result<T> {
        let result = self.sql.transaction(body).await;
        self.settle(result).await
    }
}

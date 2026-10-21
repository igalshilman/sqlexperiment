//! Extensions: what a program uses beside its memory and its scope's
//! database, under one contract with the run's commit.
//!
//! An extension, offered as `name`, gives programs up to three things:
//!
//! - **Calls in the turn**, `<name>.<op>`: the guest's host calls, answered
//!   now. They may change only what the turn has not committed yet: the
//!   turn's tables, or the extension's own uncommitted state.
//! - **Operations**, `<name>` or `<name>.<op>`: issued in a turn, made by the
//!   host after the commit that issued them (at least once, under the same
//!   ID), and delivered in a later turn. An operation has three hooks:
//!   [`Attached::issued`], in its turn; [`Attached::operation`], as it is
//!   made; [`Attached::delivered`], in the turn its outcome arrives, before
//!   the program sees it.
//! - **State kept by the turn's transaction**: an attached extension is a
//!   [`Participant`]: it keeps its state in its [`Record`] (named as the
//!   extension), its large state in the scope's blobs, which the record
//!   names. The next attempt, on any machine, reads the record back from
//!   [`Extension::attach`].
//!
//! What a seal keeps of an extension belongs to the scope, as its tables
//! do: it carries over from one run to the next.
use crate::{
    GuestError, Operation, Outcome,
    db::Db,
    transaction::{Blobs, Participant, Record, ScopeBlobs},
};
use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// One operation, as the host makes it.
pub struct HostCall {
    /// The operation, `<run>/<n>`: the same on every attempt.
    pub id: String,
    /// As the program issued it: `<extension>` or `<extension>.<op>`.
    pub name: String,
    /// The program's idempotency key, if it set one, or else `id`.
    pub idempotency_key: String,
    /// The input, as the program's heap holds it now.
    pub input: Value,
}

/// An operation, made: its outcome, when it comes.
pub type Making = BoxFuture<'static, Result<Value, GuestError>>;

/// The scope's tables: in a turn, in its transaction; as an operation is
/// made, as the last commit left them.
pub struct Tables<'a> {
    pub(crate) db: &'a mut Db,
}

impl Tables<'_> {
    /// The rows `sql` selects with `params`, its values as `ctx.db` has them.
    pub async fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        self.db.query(sql, params).await
    }

    /// Run `sql` with `params`.
    pub async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<()> {
        self.db.query(sql, params).await.map(drop)
    }
}

/// An extension, as a deployment offers it.
#[async_trait]
pub trait Extension: Send + Sync + 'static {
    /// Its part of a run in the scope `blobs` are of: as the scope's last
    /// seal left its `record` (empty if none did), or new.
    async fn attach(&self, blobs: ScopeBlobs, record: Record) -> Result<Box<dyn Attached>>;
}

/// An extension's part of one attempt at a run: a participant in each of
/// its turns' transactions.
#[async_trait]
pub trait Attached: Participant {
    /// In the turn: the host call `op`, `<name>.<op>`.
    async fn call(
        &mut self,
        _tables: &mut Tables<'_>,
        op: &str,
        _input: Value,
    ) -> Result<Value, GuestError> {
        Err(GuestError::new("TypeError", format!("No call {op}")))
    }

    /// In the turn: the program issued `operation`. An error rejects it now.
    fn issued(&mut self, _operation: &Operation) -> Result<(), GuestError> {
        Ok(())
    }

    /// Whether an operation made again under the same idempotency key is
    /// answered as the first was. Only then may the program set a key.
    fn deduplicates(&self) -> bool {
        false
    }

    /// After the commit: make `call`, reading what it needs from `tables` (as
    /// the commit left them) now.
    async fn operation(
        &mut self,
        call: HostCall,
        _tables: &mut Tables<'_>,
    ) -> Result<Making, GuestError> {
        Err(GuestError::new(
            "TypeError",
            format!("No operation {}", call.name),
        ))
    }

    /// In the turn `operation`'s outcome arrives, before the program sees it:
    /// keep what it brought, and change what the program sees.
    async fn delivered(
        &mut self,
        _tables: &mut Tables<'_>,
        _operation: &Operation,
        _outcome: &mut Outcome,
    ) -> Result<()> {
        Ok(())
    }

    /// The run waits, and may move: release what is live.
    fn park(&mut self) {}
}

/// The runtime's own participants' names, which name their records.
pub(crate) const BUILT_IN: [&str; 3] = ["run", "memory", "tables"];

/// The extensions a deployment offers its programs, by name, and where
/// they keep what is too large for a seal.
#[derive(Clone, Default)]
pub struct Extensions {
    offered: BTreeMap<String, Arc<dyn Extension>>,
    pub(crate) blobs: Option<Arc<dyn Blobs>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep scopes' blobs in `blobs`.
    pub fn with_blobs(mut self, blobs: impl Blobs) -> Self {
        self.blobs = Some(Arc::new(blobs));
        self
    }

    /// Offer `extension` as `name`: its calls are `<name>.<op>`.
    pub fn with(mut self, name: impl Into<String>, extension: impl Extension) -> Self {
        let name = name.into();
        assert!(
            !name.is_empty() && !name.contains('.') && !BUILT_IN.contains(&name.as_str()),
            "Extension name {name:?}"
        );
        self.offered.insert(name, Arc::new(extension));
        self
    }
}

/// The extensions' parts of one attempt at a run, by name.
#[derive(Default)]
pub(crate) struct Attachments(BTreeMap<String, Box<dyn Attached>>);

impl Attachments {
    /// Each of `extensions`, in the scope `db` is of, with its record.
    pub async fn attach(extensions: &Extensions, blobs: &ScopeBlobs, db: &Db) -> Result<Self> {
        let mut attached = BTreeMap::new();
        for (name, extension) in &extensions.offered {
            let record = db.journal().record(name);
            let part = extension.attach(blobs.clone(), record).await?;
            attached.insert(name.clone(), part);
        }
        Ok(Self(attached))
    }

    /// The extension a call or an operation, `<name>[.<op>]`, is to.
    pub fn get(&mut self, called: &str) -> Option<&mut Box<dyn Attached>> {
        let name = called.split_once('.').map_or(called, |(name, _)| name);
        self.0.get_mut(name)
    }

    /// Each extension's part of the seal.
    pub async fn prepare(&mut self) -> Result<()> {
        for part in self.0.values_mut() {
            part.prepare().await?;
        }
        Ok(())
    }

    pub fn committed(&mut self) {
        self.0.values_mut().for_each(|part| part.committed());
    }

    pub fn aborted(&mut self) {
        self.0.values_mut().for_each(|part| part.aborted());
    }

    /// The blobs each extension's record names (`None` for one with some in
    /// flight).
    pub fn holds(&self) -> Vec<Option<BTreeSet<String>>> {
        self.0.values().map(|part| part.holds()).collect()
    }

    pub fn park(&mut self) {
        self.0.values_mut().for_each(|part| part.park());
    }
}

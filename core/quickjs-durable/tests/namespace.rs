//! The SQLite host's tables in a sqlite-kv namespace: each turn is one session,
//! committed by one save, or dropped.
#![cfg(feature = "sqlite-kv-host")]
use anyhow::Result;
use bytes::Bytes;
use common::programs;
use futures::future::BoxFuture;
use futures_executor::block_on;
use quickjs_durable::{
    Engine, Operation, Outcome, Resolution, Status, Transaction, namespace::NamespaceTransaction,
};
use serde_json::{Value, json};
use sqlite_kv::{StorageOptions, kv::KvStore};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

mod common;

/// A reliable in-memory store: a Restate object's state, in miniature.
#[derive(Default)]
struct Store(Mutex<HashMap<String, Bytes>>);
impl KvStore for Store {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().get(key).cloned()) })
    }
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().insert(key.into(), value);
            Ok(())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.lock().unwrap().remove(key);
            Ok(())
        })
    }
}

const SOURCE: &str = r#"async function main(input, ctx) {
    const n = (await ctx.kv.get('count')) ?? 0;
    await ctx.kv.set('count', n + input);
    const doubled = await ctx.rpc.call({service: 'Worker', handler: 'double'}, n + input);
    await ctx.kv.set('last', doubled);
    return doubled;
}"#;

/// A new session of the namespace, for a turn of `run`.
fn open(store: &Arc<Store>, run: &str) -> Result<NamespaceTransaction> {
    block_on(NamespaceTransaction::begin(
        store.clone(),
        StorageOptions::default(),
        run,
    ))
}

/// A turn of `run` of SOURCE: an engine on a new session of the namespace.
fn begin(store: &Arc<Store>, run: &str) -> Result<Engine<NamespaceTransaction>> {
    Ok(Engine::new(open(store, run)?, programs(SOURCE)))
}

fn commit(engine: Engine<NamespaceTransaction>) -> Result<()> {
    block_on(engine.into_storage()?.commit())
}

fn effects(store: &Arc<Store>, run: &str) -> Result<Vec<Operation>> {
    block_on(open(store, run)?.effects())
}

fn state(store: &Arc<Store>, key: &str) -> Result<Value> {
    // A guest's read, in a turn that is then dropped.
    let operation = Operation::KvGet {
        id: "reader/0".into(),
        key: key.into(),
    };
    match block_on(open(store, "reader")?.execute(&operation))? {
        Resolution::Ready(Outcome::Success { value, .. }) => Ok(value),
        _ => anyhow::bail!("Unexpected KV result"),
    }
}

#[test]
fn turns_commit_one_save_each_and_effects_wait_in_the_outbox() -> Result<()> {
    let store = Arc::new(Store::default());
    let mut e = begin(&store, "run")?;
    let start = block_on(e.start("run", "v1", json!(20), 1000))?;
    assert_eq!(start.state.status, Status::Waiting);
    commit(e)?;

    let outbox = effects(&store, "run")?;
    assert_eq!(outbox.len(), 1);
    let Operation::Rpc { id, input, .. } = &outbox[0] else {
        panic!("Expected an RPC, got {:?}", outbox[0]);
    };
    assert_eq!(input, &json!(20));
    assert_eq!(state(&store, "count")?, json!(20));

    // The inbox delivers the result in a later turn, on a cold namespace.
    let mut e = begin(&store, "run")?;
    let done = block_on(e.complete("run", id, Outcome::success(40), 1001))?;
    assert_eq!(done.state.status, Status::Completed);
    commit(e)?;
    assert!(effects(&store, "run")?.is_empty());
    assert_eq!(state(&store, "last")?, json!(40));

    // A second run shares the scope.
    let mut e = begin(&store, "second")?;
    let second = block_on(e.start("second", "v1", json!(1), 1002))?;
    assert_eq!(second.state.pending.len(), 1);
    commit(e)?;
    assert_eq!(state(&store, "count")?, json!(21));
    let recovered = block_on(begin(&store, "run")?.recover("run"))?;
    assert_eq!(recovered.checkpoint_id, done.checkpoint_id);
    Ok(())
}

#[test]
fn a_dropped_turn_leaves_the_committed_one() -> Result<()> {
    let store = Arc::new(Store::default());
    let mut e = begin(&store, "run")?;
    let start = block_on(e.start("run", "v1", json!(5), 1000))?;
    commit(e)?;
    let rpc = start.state.pending[0].id().to_owned();

    // Completed, but never committed: the crash happened before the save.
    let mut e = begin(&store, "run")?;
    block_on(e.complete("run", &rpc, Outcome::success(10), 1001))?;
    drop(e);

    let recovered = block_on(begin(&store, "run")?.recover("run"))?;
    assert_eq!(recovered.checkpoint_id, start.checkpoint_id);
    assert_eq!(effects(&store, "run")?[0].id(), rpc);
    assert_eq!(state(&store, "last")?, Value::Null);

    // Redelivered, it completes once.
    let mut e = begin(&store, "run")?;
    let done = block_on(e.complete("run", &rpc, Outcome::success(10), 1002))?;
    commit(e)?;
    assert_eq!(done.state.status, Status::Completed);
    let mut e = begin(&store, "run")?;
    let duplicate = block_on(e.complete("run", &rpc, Outcome::success(10), 1003))?;
    assert_eq!(duplicate.checkpoint_id, done.checkpoint_id);
    Ok(())
}

#[test]
fn a_turn_belongs_to_one_run() -> Result<()> {
    let store = Arc::new(Store::default());
    let mut e = begin(&store, "run")?;
    let start = block_on(e.start("run", "v1", json!(1), 1000))?;
    commit(e)?;
    let rpc = start.state.pending[0].id().to_owned();
    let mut other = begin(&store, "other")?;
    assert!(block_on(other.complete("run", &rpc, Outcome::success(2), 1001)).is_err());
    assert!(block_on(begin(&store, "other")?.recover("run")).is_err());
    Ok(())
}

#[test]
fn a_trap_abandons_the_runs_outbox() -> Result<()> {
    let store = Arc::new(Store::default());
    let trap = programs(
        "async function main(_,ctx){ctx.rpc.call({service:'S',handler:'h'});await ctx.kv.set('n',1);while(true){}}",
    );
    let mut e = Engine::new(open(&store, "run")?, trap);
    let failed = block_on(e.start("run", "v1", Value::Null, 1000))?;
    assert_eq!(failed.state.status, Status::Failed);
    commit(e)?;
    assert!(effects(&store, "run")?.is_empty());
    assert_eq!(state(&store, "n")?, json!(1));
    Ok(())
}

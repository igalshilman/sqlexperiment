//! What the integration tests share: one runtime, catalogs compiled once, and
//! a driver that answers a run's requests until it settles.
#![allow(dead_code)]
use anyhow::Result;
use quickjs_durable::{MemoryStore, Operation, Outcome, Programs, Run, Runtime, Store, View};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

/// The runtime every catalog of a test binary is on: compiling it is slow.
pub fn runtime() -> Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| Arc::new(Runtime::new().expect("compile QuickJS")))
        .clone()
}

/// A catalog with `source` as program "v1", compiled once per source.
pub fn programs(source: &str) -> Arc<Programs> {
    static PROGRAMS: OnceLock<Mutex<HashMap<String, Arc<Programs>>>> = OnceLock::new();
    let mut catalogs = PROGRAMS.get_or_init(Default::default).lock().unwrap();
    catalogs
        .entry(source.into())
        .or_insert_with(|| {
            Arc::new(
                Programs::new(runtime())
                    .compile("v1", source)
                    .expect("compile v1"),
            )
        })
        .clone()
}

pub fn block<F: Future>(future: F) -> F::Output {
    futures_executor::block_on(future)
}

/// Run "r" of `source`'s program "v1" in `store`: started, or another attempt.
pub fn start(store: &MemoryStore, source: &str, input: Value, now: u64) -> Result<Run> {
    block(Run::start(&programs(source), store, "r", "v1", input, now))
}

/// The value the run completed with.
pub fn value(view: &View) -> Value {
    match &view.state.outcome {
        Some(Outcome::Success { value, .. }) => value.clone(),
        other => panic!("Unexpected outcome {other:?}"),
    }
}

/// The scope's KV value of `key`.
pub fn kv(store: &MemoryStore, key: &str) -> Option<Value> {
    block(store.get(&format!("kv/{key}")))
        .unwrap()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
}

/// Keys stored under `prefix`.
pub fn stored(store: &MemoryStore, prefix: &str) -> usize {
    store
        .values()
        .keys()
        .filter(|key| key.starts_with(prefix))
        .count()
}

/// The answer the tests' host gives each request: an RPC's input back, with
/// the request's ID; null for anything else.
pub fn answer(operation: &Operation) -> Outcome {
    match operation {
        Operation::Rpc { id, input, .. } => Outcome::success(json!({"id": id, "input": input})),
        _ => Outcome::success(Value::Null),
    }
}

/// Deliver the first pending request's answer, at its deadline for a timer.
pub fn step(run: &mut Run, store: &MemoryStore) -> Result<View> {
    let state = run.view().state;
    let operation = state.pending.first().expect("a pending request").clone();
    let now = match &operation {
        Operation::Sleep { deadline, .. } => *deadline,
        _ => state.now + 1,
    };
    block(run.deliver(store, operation.id(), answer(&operation), now))
}

/// Answer the run's requests until it settles.
pub fn finish(run: &mut Run, store: &MemoryStore) -> Result<View> {
    while run.view().state.outcome.is_none() {
        step(run, store)?;
    }
    Ok(run.view())
}

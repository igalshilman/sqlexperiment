//! What the integration tests share: one runtime, catalogs compiled once, and
//! a driver that answers a run's requests until it settles.
#![allow(dead_code)]
use anyhow::Result;
use functions::{
    Extensions, GuestError, Language, Limits, Operation, Outcome, Programs, Run, Runtime, Scope,
    View,
};
use serde_json::{Value, json};
use sql::{Sql, Value as SqlValue};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use volume::{Volume, kv::KvStore, simulation::MemoryKv};

/// A scope's store in memory: what the tests read back from it.
#[derive(Clone, Default)]
pub struct Store {
    pub kv: Arc<MemoryKv>,
}

impl Store {
    pub fn scope(&self) -> Scope {
        Scope::new(self.kv.clone() as Arc<dyn KvStore>)
    }

    /// Every key and its value.
    pub fn values(&self) -> BTreeMap<String, Vec<u8>> {
        (self.kv.keys().into_iter())
            .map(|key| {
                let value = block(self.kv.get(&key)).unwrap().unwrap();
                (key, value.to_vec())
            })
            .collect()
    }

    /// Bytes put so far, keys included.
    pub fn bytes_written(&self) -> usize {
        self.kv.stats().bytes_written as usize
    }

    fn open(&self) -> Sql {
        let volume = block(Volume::open(self.kv.clone())).unwrap();
        block(Sql::open(&volume)).unwrap()
    }

    /// The record `name` (`run`, `memory`, an extension's), as the scope's
    /// last seal holds it: JSON, or null if none set it.
    pub fn record(&self, name: &str) -> Value {
        let bytes = block(Volume::open(self.kv.clone()))
            .unwrap()
            .record(name)
            .get();
        match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap(),
        }
    }

    /// Seals written in the scope: its database's version.
    pub fn seals(&self) -> u64 {
        self.open().version()
    }

    /// Seal `value` as the record `name`, as if a turn did.
    pub fn set_record(&self, name: &str, value: &Value) {
        let volume = block(Volume::open(self.kv.clone())).unwrap();
        let bytes = serde_json::to_vec(value).unwrap();
        volume.record(name).set(bytes.into());
        block(volume.commit()).unwrap();
    }

    /// Pages of the run's memory that differ from golden, as the memory
    /// record lists them.
    pub fn memory_pages(&self) -> usize {
        let pages = self.record("memory")["pages"].clone();
        let runs = pages.as_array().cloned().unwrap_or_default();
        runs.iter()
            .map(|run| run[1].as_u64().unwrap() as usize)
            .sum()
    }

    /// Page keys in the store: pages checkpointed in place.
    pub fn page_keys(&self) -> usize {
        self.kv.page_count()
    }
}

/// The runtime every catalog of a test binary is on: compiling it is slow.
/// Its turns are short, so a job that never returns fails quickly.
pub fn runtime() -> Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let limits = Limits {
                turn: Duration::from_secs(1),
                ..Limits::default()
            };
            Arc::new(Runtime::new().expect("compile QuickJS").limited(limits))
        })
        .clone()
}

/// A catalog with JavaScript `source` as program "v1", compiled once per source.
pub fn programs(source: &str) -> Arc<Programs> {
    catalog(Language::JavaScript, source)
}

/// A catalog with `source`, in `language`, as program "v1", compiled once per
/// source.
pub fn catalog(language: Language, source: &str) -> Arc<Programs> {
    static PROGRAMS: OnceLock<Mutex<HashMap<String, Arc<Programs>>>> = OnceLock::new();
    let mut catalogs = PROGRAMS.get_or_init(Default::default).lock().unwrap();
    catalogs
        .entry(source.into())
        .or_insert_with(|| {
            let programs = Programs::new(runtime());
            Arc::new(
                match language {
                    Language::JavaScript => programs.compile("v1", source),
                    Language::Python => programs.compile_python("v1", source),
                }
                .expect("compile v1"),
            )
        })
        .clone()
}

pub fn block<F: Future>(future: F) -> F::Output {
    futures_executor::block_on(future)
}

/// Run "r" of `source`'s program "v1" in `store`: started, or another attempt.
pub fn start(store: &Store, source: &str, input: Value, now: u64) -> Result<Run> {
    block(Run::start(
        &programs(source),
        &store.scope(),
        "s",
        "r",
        "v1",
        "main",
        input,
        now,
    ))
}

/// Run "r" of Python `source`'s program "v1" in `store`: started, or another
/// attempt.
pub fn start_python(store: &Store, source: &str, input: Value, now: u64) -> Result<Run> {
    block(Run::start(
        &catalog(Language::Python, source),
        &store.scope(),
        "s",
        "r",
        "v1",
        "main",
        input,
        now,
    ))
}

/// The call `run` of `handler` on the scope "s" of `source`'s program "v1".
pub fn call(store: &Store, source: &str, run: &str, handler: &str, input: Value) -> Result<Run> {
    let now = 1;
    block(Run::start(
        &programs(source),
        &store.scope(),
        "s",
        run,
        "v1",
        handler,
        input,
        now,
    ))
}

/// The value the run completed with.
pub fn value(view: &View) -> Value {
    match &view.state.outcome {
        Some(Outcome::Success { value, .. }) => value.clone(),
        other => panic!("Unexpected outcome {other:?}"),
    }
}

/// The scope's KV value of `key`.
pub fn kv(store: &Store, key: &str) -> Option<Value> {
    let mut db = store.open();
    let select = "SELECT value FROM kv WHERE key = ?";
    let rows = block(db.query_with(select, [SqlValue::build_text(key.to_owned())])).ok()?;
    match rows.first()?.first()? {
        SqlValue::Text(text) => Some(serde_json::from_str(text.as_str()).unwrap()),
        other => panic!("KV value {other:?}"),
    }
}

/// The answer the tests' host gives each request: an RPC's input back (read
/// from the guest's heap, as the host reads it to make the call), with the
/// request's ID; null for anything else.
pub fn answer(run: &mut Run, operation: &Operation) -> Outcome {
    match operation {
        Operation::Rpc { id, .. } => {
            let input = block(run.input(id)).expect("the input");
            Outcome::from_result(input.map(|input| json!({"id": id, "input": input})))
        }
        _ => Outcome::success(Value::Null),
    }
}

/// Deliver the first pending request's answer, at its deadline for a timer.
pub fn step(run: &mut Run) -> Result<View> {
    let state = run.view().state;
    let operation = state.pending.first().expect("a pending request").clone();
    let now = match &operation {
        Operation::Sleep { deadline, .. } => *deadline,
        _ => state.now + 1,
    };
    let answer = answer(run, &operation);
    block(run.deliver(operation.id(), answer, now))
}

/// Answer the run's requests until it settles.
pub fn finish(run: &mut Run) -> Result<View> {
    while run.view().state.outcome.is_none() {
        step(run)?;
    }
    Ok(run.view())
}

/// An attempt at run "r" of `catalog`'s program "v1", `main(input)`, in
/// `store`, offered `extensions`: the run started, or the one its last
/// commit holds.
pub async fn attempt(
    store: &Store,
    catalog: &Arc<Programs>,
    input: Value,
    extensions: &Extensions,
) -> Run {
    let scope = store.scope().extended(extensions.clone());
    Run::start(catalog, &scope, "s", "r", "v1", "main", input, 1)
        .await
        .unwrap()
}

/// Make the extension's operation `id`, as the host would: its outcome.
pub async fn make(run: &mut Run, id: &str) -> Result<Value, GuestError> {
    run.operation(id).await.unwrap()?.await
}

/// Run `main(input)` as [`attempt`] does, making its first pending
/// operation and delivering it (a timer at its deadline), until the run
/// settles: its value, or `{"failed": message}`.
pub async fn drive(
    store: &Store,
    catalog: &Arc<Programs>,
    input: Value,
    extensions: &Extensions,
) -> Value {
    let mut run = attempt(store, catalog, input, extensions).await;
    settle(&mut run).await
}

/// Make and deliver `run`'s first pending operation, until it settles.
pub async fn settle(run: &mut Run) -> Value {
    loop {
        let view = run.view();
        if let Some(outcome) = view.state.outcome {
            return match outcome {
                Outcome::Success { value, .. } => value,
                Outcome::Failure { error, .. } => json!({"failed": error.message}),
            };
        }
        let operation = view.state.pending[0].clone();
        let (outcome, now) = match &operation {
            Operation::Sleep { deadline, .. } => (Outcome::success(Value::Null), *deadline),
            _ => {
                let made = make(run, operation.id()).await;
                (Outcome::from_result(made), view.state.now + 1)
            }
        };
        run.deliver(operation.id(), outcome, now).await.unwrap();
    }
}

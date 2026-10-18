//! A run: one invocation of a program's `main`, turn by turn. A turn delivers
//! one event to the VM, runs the JavaScript until it waits, and commits.
//!
//! A scope's storage is three kinds of key: `commit`, its current run as of
//! its last turn ([`Commit`]); `batch/<n>`, immutable batches of changed
//! pages; and `kv/<key>`, the scope's KV. The image is a chain of batches over
//! the program's golden image: the first holds the pages that differ from
//! golden, each next one the pages one turn changed, each compressed against
//! what it replaces, so a turn writes little more than what it changed. A turn
//! puts its batch, then `commit`: the turn happened once `commit` is
//! stored, and a crash that loses a suffix of the writes never keeps a commit
//! without its batch. Then it applies what the commit lists: the turn's KV
//! writes, and deleting the batches no longer referenced. The next attempt
//! that reads the commit applies them again, so a crash in between loses
//! nothing; a crash before the commit loses the whole turn, which runs again.
//!
//! The VM stays live from turn to turn of one attempt; a new attempt restores
//! it from the commit, once, when a delivery needs it.
use crate::{
    Store, codec,
    model::*,
    program::{Program, Programs},
    vm::{self, Vm},
};
use anyhow::{Context, Result, ensure};
use futures::future;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

const COMMIT: &str = "commit";
const MAX_RECORD: usize = 16 * 1024 * 1024;
/// Most batches in an image's chain. Restoring reads them all at once and
/// applies them in order; past this many, or once the later batches outweigh
/// twice the first, a turn writes every page that differs from golden into a
/// new first batch, so restoring stays bounded however long a run lives. The
/// size rule keeps rewriting to a third of what a run writes; this one only
/// bounds the reads, as each new first batch costs a whole image.
const MAX_CHAIN: usize = 256;

fn batch_key(batch: u64) -> String {
    format!("batch/{batch}")
}
fn kv_key(key: &str) -> String {
    format!("kv/{key}")
}
/// The pages at `indexes`, back to back.
fn pages<'a>(page: impl Fn(u32) -> &'a [u8], indexes: &[u32]) -> Vec<u8> {
    let mut pages = Vec::with_capacity(indexes.len() * PAGE);
    for &index in indexes {
        pages.extend_from_slice(page(index));
    }
    pages
}
fn input_size<T: serde::Serialize>(input: &T) -> Result<()> {
    ensure!(json_bytes(input)?.len() <= MAX_INPUT, "Input exceeds limit");
    Ok(())
}

/// The pages a turn changed, and what they held before it.
#[derive(Default)]
struct Changed {
    indexes: Vec<u32>,
    old: Vec<u8>,
    new: Vec<u8>,
}

/// A run of a program in one scope, in one attempt. An error leaves it
/// unusable: the attempt starts again, from the store.
pub struct Run {
    programs: Arc<Programs>,
    commit: Commit,
    /// Batches of the scope's previous run, for the first commit to collect.
    inherited: Vec<u64>,
    /// The VM, and its memory as of the last commit: restored when a delivery
    /// needs it, and dropped once the run has settled.
    live: Option<Live>,
    last: Written,
    usable: bool,
}

struct Live {
    vm: Vm,
    base: Image,
}

/// What a turn wrote.
#[derive(Default)]
struct Written {
    changed_pages: usize,
    examined_pages: usize,
    batch_bytes: usize,
    commit_bytes: usize,
}

enum Delivery {
    /// The run's first turn: `main(input)`, in `scope`.
    Start { scope: String, input: Value },
    /// Requests' outcomes, delivered in order before the JavaScript runs.
    Complete(Vec<(String, Outcome)>),
}

impl Run {
    /// Start run `run` of `program` on `input`, in `scope` (the program's
    /// `ctx.key`): its first turn runs `main` until it waits or settles, and
    /// commits. When the scope's commit is
    /// already this run's, this is another attempt at it: the run continues as
    /// committed, and `input` is the one it started with.
    pub async fn start(
        programs: &Arc<Programs>,
        store: &impl Store,
        scope: &str,
        run: &str,
        program: &str,
        input: Value,
        now: u64,
    ) -> Result<Self> {
        ensure!(
            !run.is_empty() && run.len() <= 1024 && !run.contains('/'),
            "Invalid run ID"
        );
        validate_time(now)?;
        input_size(&input)?;
        let previous = load(store).await?;
        if let Some(commit) = &previous {
            apply(store, commit).await?;
        }
        if let Some(commit) = previous.as_ref().filter(|commit| commit.run == run) {
            ensure!(
                commit.program == program,
                "Run {run} is of program {}",
                commit.program
            );
            check(programs, commit)?;
            return Ok(Self {
                programs: programs.clone(),
                commit: previous.unwrap(),
                inherited: Vec::new(),
                live: None,
                last: Written::default(),
                usable: true,
            });
        }
        let golden = programs.get(program)?.image();
        let mut vm = Vm::new(programs.runtime(), run, now)?;
        vm.restore(&golden)?;
        let commit = Commit {
            run: run.into(),
            program: program.into(),
            version: programs.get(program)?.version(),
            runtime: vm::VERSION,
            turns: 0,
            meta: golden.meta.clone(),
            memory_len: golden.len(),
            chain: Vec::new(),
            base_bytes: 0,
            delta_bytes: 0,
            live_pages: 0,
            next_batch: previous.as_ref().map_or(0, |commit| commit.next_batch),
            state: State {
                pending: Vec::new(),
                next_operation: 0,
                now,
                status: Status::Waiting,
                outcome: None,
            },
            writes: BTreeMap::new(),
            deletes: Vec::new(),
            garbage: Vec::new(),
        };
        let mut this = Self {
            programs: programs.clone(),
            commit,
            inherited: previous.map_or_else(Vec::new, |commit| commit.chain),
            live: Some(Live { vm, base: golden }),
            last: Written::default(),
            usable: false,
        };
        let start = Delivery::Start {
            scope: scope.into(),
            input,
        };
        this.turn(store, start, now).await?;
        this.usable = true;
        Ok(this)
    }

    /// Deliver the outcome of the pending request `operation` at `now`, and
    /// run a turn. A request delivered before, or dropped when the run
    /// settled, changes nothing.
    pub async fn deliver(
        &mut self,
        store: &impl Store,
        operation: &str,
        outcome: Outcome,
        now: u64,
    ) -> Result<View> {
        self.deliver_all(store, vec![(operation.to_owned(), outcome)], now)
            .await
    }

    /// Deliver the outcomes of several pending requests at `now`, in order,
    /// and run one turn: what arrived together commits once. Requests
    /// delivered before, or dropped when the run settled, are skipped.
    pub async fn deliver_all(
        &mut self,
        store: &impl Store,
        outcomes: Vec<(String, Outcome)>,
        now: u64,
    ) -> Result<View> {
        ensure!(
            self.usable,
            "This attempt at the run failed: start it again"
        );
        validate_time(now)?;
        let state = &self.commit.state;
        let now = now.max(state.now);
        let mut delivered = HashSet::new();
        let mut pending = Vec::new();
        for (operation, outcome) in outcomes {
            outcome.validate()?;
            input_size(&outcome)?;
            ensure!(delivered.insert(operation.clone()), "Duplicate outcome");
            let Some(request) = state.pending.iter().find(|p| p.id() == operation) else {
                let issued = operation
                    .strip_prefix(&format!("{}/", self.commit.run))
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| {
                        n < state.next_operation && operation == format!("{}/{n}", self.commit.run)
                    });
                ensure!(issued, "Unknown operation ID");
                continue;
            };
            if let Operation::Sleep { deadline, .. } = request {
                ensure!(now >= *deadline, "Timer completed before deadline");
            }
            pending.push((operation, outcome));
        }
        if pending.is_empty() {
            return Ok(self.view());
        }
        self.usable = false;
        if self.live.is_none() {
            self.live = Some(self.restore(store, now).await?);
        }
        self.turn(store, Delivery::Complete(pending), now).await?;
        self.usable = true;
        Ok(self.view())
    }

    /// The run as of its last commit.
    pub fn view(&self) -> View {
        let commit = &self.commit;
        View {
            run: commit.run.clone(),
            program: commit.program.clone(),
            turns: commit.turns,
            state: commit.state.clone(),
            memory_bytes: commit.memory_len,
            live_pages: commit.live_pages,
            changed_pages: self.last.changed_pages,
            examined_pages: self.last.examined_pages,
            batch_bytes: self.last.batch_bytes,
            commit_bytes: self.last.commit_bytes,
        }
    }

    /// The committed image, from golden and the batches, in a new VM.
    async fn restore(&self, store: &impl Store, now: u64) -> Result<Live> {
        let commit = &self.commit;
        let mut base = check(&self.programs, commit)?.image();
        ensure!(
            commit.memory_len >= base.len()
                && commit.memory_len <= MAX_MEMORY
                && commit.memory_len.is_multiple_of(65536),
            "Invalid committed memory size"
        );
        base.meta = commit.meta.clone();
        base.grow(commit.memory_len);
        let batches = future::try_join_all(
            commit
                .chain
                .iter()
                .map(|batch| async move { store.get(&batch_key(*batch)).await }),
        )
        .await?;
        for bytes in batches {
            let bytes = bytes.context("Missing page batch")?;
            let (indexes, frame) = codec::page_indexes(&bytes)?;
            ensure!(
                indexes
                    .last()
                    .is_none_or(|&last| (last as usize + 1) * PAGE <= base.len()),
                "Page beyond memory"
            );
            let decoded = codec::decode_pages(frame, &pages(|i| base.page(i), &indexes))?;
            for (index, page) in indexes.iter().zip(decoded.chunks_exact(PAGE)) {
                base.set(*index, page);
            }
        }
        let mut vm = Vm::new(self.programs.runtime(), &commit.run, now)?;
        vm.restore(&base)?;
        Ok(Live { vm, base })
    }

    /// Run the JavaScript on `delivery`, capture what changed, and commit.
    async fn turn(&mut self, store: &impl Store, delivery: Delivery, now: u64) -> Result<()> {
        let live = self.live.as_mut().context("No live VM")?;
        let mut writes = BTreeMap::new();
        let ran = run(
            &mut live.vm,
            store,
            &self.commit,
            delivery,
            now,
            &mut writes,
        )
        .await;
        let mut written = Written::default();
        let mut live_pages = self.commit.live_pages;
        let (state, meta, memory_len, changed) = match ran {
            Ok(state) => {
                let capture = live.vm.capture(&live.base)?;
                written.changed_pages = capture.pages.len();
                written.examined_pages = capture.examined_pages;
                let base = &mut live.base;
                base.meta = capture.meta.clone();
                base.grow(capture.memory_len);
                let mut changed = Changed::default();
                for (index, new) in capture.pages {
                    changed.indexes.push(index);
                    changed.old.extend_from_slice(base.page(index));
                    changed.new.extend_from_slice(&new);
                    base.set(index, &new);
                }
                live_pages = base.changed().count();
                (state, capture.meta, capture.memory_len, changed)
            }
            // The guest trapped: its image is unusable, and the same delivery
            // would trap again. The run ends; the KV writes before the trap
            // stand, as for a thrown error.
            Err(error) => match vm::trap(&error) {
                Some(message) => {
                    self.live = None;
                    let state = State {
                        pending: Vec::new(),
                        next_operation: self.commit.state.next_operation,
                        now,
                        status: Status::Failed,
                        outcome: Some(Outcome::failure("Trap", message)),
                    };
                    let commit = &self.commit;
                    (
                        state,
                        commit.meta.clone(),
                        commit.memory_len,
                        Changed::default(),
                    )
                }
                None => return Err(error.into()),
            },
        };
        let previous = &self.commit;
        let mut chain = previous.chain.clone();
        let (mut base_bytes, mut delta_bytes) = (previous.base_bytes, previous.delta_bytes);
        let mut next_batch = previous.next_batch;
        let settled = state.status != Status::Waiting;
        if settled {
            // A settled run never restores again: it keeps its outcome, not
            // its image.
            chain.clear();
            live_pages = 0;
        } else if !changed.indexes.is_empty() {
            let mut bytes = codec::encode_pages(&changed.indexes, &changed.new, &changed.old)?;
            if chain.is_empty() {
                base_bytes = bytes.len();
            } else if chain.len() >= MAX_CHAIN || delta_bytes + bytes.len() > 2 * base_bytes {
                // A new first batch: every page that differs from golden.
                let base = &self.live.as_ref().context("No live VM")?.base;
                let live: Vec<u32> = base.changed().collect();
                bytes = codec::encode_pages(
                    &live,
                    &pages(|i| base.page(i), &live),
                    &pages(|i| base.original(i), &live),
                )?;
                chain.clear();
                (base_bytes, delta_bytes) = (bytes.len(), 0);
            } else {
                delta_bytes += bytes.len();
            }
            written.batch_bytes = bytes.len();
            store.put(&batch_key(next_batch), bytes).await?;
            chain.push(next_batch);
            next_batch += 1;
        }
        let garbage = previous
            .chain
            .iter()
            .chain(&self.inherited)
            .filter(|batch| !chain.contains(batch))
            .copied()
            .collect();
        let mut kv_writes = BTreeMap::new();
        let mut deletes = Vec::new();
        for (key, value) in writes {
            match value {
                Some(value) => {
                    kv_writes.insert(key, value);
                }
                None => deletes.push(key),
            }
        }
        let commit = Commit {
            run: previous.run.clone(),
            program: previous.program.clone(),
            version: previous.version,
            runtime: previous.runtime,
            turns: previous.turns + 1,
            meta,
            memory_len,
            chain,
            base_bytes,
            delta_bytes,
            live_pages,
            next_batch,
            state,
            writes: kv_writes,
            deletes,
            garbage,
        };
        let bytes = json_bytes(&commit)?;
        ensure!(bytes.len() <= MAX_RECORD, "Commit exceeds limit");
        written.commit_bytes = bytes.len();
        store.put(COMMIT, bytes).await?;
        apply(store, &commit).await?;
        self.commit = commit;
        self.inherited.clear();
        self.last = written;
        match &mut self.live {
            Some(_) if settled => self.live = None,
            Some(live) => live.vm.committed()?,
            None => {}
        }
        Ok(())
    }
}

/// Deliver to the VM, and run the JavaScript until it waits on what the host
/// answers (RPCs, timers, wakeups), settles, or spends half its fuel. KV
/// requests are answered here, in the turn: a read sees the turn's own writes.
async fn run(
    vm: &mut Vm,
    store: &impl Store,
    commit: &Commit,
    delivery: Delivery,
    now: u64,
    writes: &mut BTreeMap<String, Option<Value>>,
) -> wasmtime::Result<State> {
    vm.begin_turn(now)?;
    // The host keeps the requests pending: those of earlier turns, less the
    // ones delivered, then those the guest issues, as it reports them.
    let mut pending = commit.state.pending.clone();
    match delivery {
        Delivery::Start { scope, input } => {
            let seed = fingerprint(commit.run.as_bytes()) as u32;
            let init = json!({
                "runId": commit.run,
                "scope": scope,
                "now": now,
                "maxTime": MAX_TIME,
                "seed": seed,
            });
            vm.invoke("init", &init)?;
            vm.invoke("start", &input)?;
        }
        Delivery::Complete(outcomes) => {
            for (operation, outcome) in outcomes {
                pending.retain(|pending| pending.id() != operation);
                vm.invoke("deliver", &completion(&operation, outcome, now))?;
            }
        }
    }
    let report = loop {
        vm.drain()?;
        let report = vm.inspect()?;
        let mut answered = false;
        for operation in report.issued.iter().cloned() {
            let value = match &operation {
                Operation::KvGet { key, .. } => match writes.get(key) {
                    Some(value) => value.clone().unwrap_or(Value::Null),
                    None => read(store, key)
                        .await
                        .map_err(wasmtime::Error::from_anyhow)?,
                },
                Operation::KvSet { key, value, .. } => {
                    writes.insert(key.clone(), Some(value.clone()));
                    Value::Null
                }
                Operation::KvDelete { key, .. } => {
                    writes.insert(key.clone(), None);
                    Value::Null
                }
                _ => {
                    pending.push(operation);
                    continue;
                }
            };
            let outcome = Outcome::success(value);
            vm.invoke("deliver", &completion(operation.id(), outcome, now))?;
            answered = true;
        }
        if !answered {
            break report;
        }
        if vm.tired()? {
            // Every request so far is answered, and the answers are in the
            // heap and the job queue; no Wasm call is live. A wakeup, which
            // the host delivers in the next turn, runs the queued jobs then.
            vm.call("pause", None)?;
            let report = vm.inspect()?;
            wasmtime::ensure!(
                matches!(&report.issued[..], [Operation::Resume { .. }]),
                "Missing resume wakeup"
            );
            pending.extend(report.issued.iter().cloned());
            break report;
        }
    };
    // A settled run drops its requests: their continuations never run.
    if report.status != Status::Waiting {
        pending.clear();
    }
    let state = State {
        pending,
        next_operation: report.next_operation,
        now: report.now,
        status: report.status,
        outcome: report.outcome,
    };
    if let Some(outcome) = &state.outcome {
        outcome.validate().map_err(wasmtime::Error::from_anyhow)?;
    }
    Ok(state)
}

fn completion(operation: &str, outcome: Outcome, now: u64) -> Value {
    json!({"kind": "complete", "operationId": operation, "outcome": outcome, "now": now})
}

/// The scope's KV value for `key`, as committed.
async fn read(store: &impl Store, key: &str) -> Result<Value> {
    let Some(bytes) = store.get(&kv_key(key)).await? else {
        return Ok(Value::Null);
    };
    ensure!(bytes.len() <= MAX_INPUT, "KV value exceeds limit");
    Ok(serde_json::from_slice(&bytes)?)
}

async fn load(store: &impl Store) -> Result<Option<Commit>> {
    let Some(bytes) = store.get(COMMIT).await? else {
        return Ok(None);
    };
    ensure!(bytes.len() <= MAX_RECORD, "Commit exceeds limit");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// What follows a commit: its KV writes, and deleting the batches it no
/// longer needs. Applying it again changes nothing.
async fn apply(store: &impl Store, commit: &Commit) -> Result<()> {
    for (key, value) in &commit.writes {
        store.put(&kv_key(key), json_bytes(value)?).await?;
    }
    for key in &commit.deletes {
        store.delete(&kv_key(key)).await?;
    }
    for batch in &commit.garbage {
        store.delete(&batch_key(*batch)).await?;
    }
    Ok(())
}

/// The run's program, as the version and runtime it started on.
fn check<'a>(programs: &'a Programs, commit: &Commit) -> Result<&'a Program> {
    let program = programs.get(&commit.program)?;
    ensure!(
        program.version() == commit.version,
        "The run started on another version of program {}",
        commit.program
    );
    ensure!(
        commit.runtime == vm::VERSION,
        "The run started on another version of the runtime"
    );
    Ok(program)
}

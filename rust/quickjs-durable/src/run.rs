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
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

const COMMIT: &str = "commit";
const MAX_RECORD: usize = 16 * 1024 * 1024;
/// Most batches in an image's chain. Restoring reads and applies them all; past
/// this many, or once the later batches outweigh twice the first, a turn
/// writes every page that differs from golden into a new first batch, so
/// restoring stays bounded however long a run lives.
const MAX_CHAIN: usize = 16;

fn batch_key(batch: u64) -> String {
    format!("batch/{batch}")
}
fn kv_key(key: &str) -> String {
    format!("kv/{key}")
}
fn page(memory: &[u8], index: usize) -> &[u8] {
    static ZERO: [u8; PAGE] = [0; PAGE];
    memory
        .get(index * PAGE..(index + 1) * PAGE)
        .unwrap_or(&ZERO)
}
/// The pages of `memory` at `indexes`, back to back.
fn pages(memory: &[u8], indexes: &[u32]) -> Vec<u8> {
    let mut pages = Vec::with_capacity(indexes.len() * PAGE);
    for &index in indexes {
        pages.extend_from_slice(page(memory, index as usize));
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
    Start(Value),
    Complete { operation: String, outcome: Outcome },
}

impl Run {
    /// Start run `run` of `program` on `input`: its first turn runs `main`
    /// until it waits or settles, and commits. When the scope's commit is
    /// already this run's, this is another attempt at it: the run continues as
    /// committed, and `input` is the one it started with.
    pub async fn start(
        programs: &Arc<Programs>,
        store: &impl Store,
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
            memory_len: golden.memory.len(),
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
        this.turn(store, Delivery::Start(input), now).await?;
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
        ensure!(
            self.usable,
            "This attempt at the run failed: start it again"
        );
        validate_time(now)?;
        outcome.validate()?;
        input_size(&outcome)?;
        let state = &self.commit.state;
        let Some(request) = state.pending.iter().find(|p| p.id() == operation) else {
            let issued = operation
                .strip_prefix(&format!("{}/", self.commit.run))
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| {
                    n < state.next_operation && operation == format!("{}/{n}", self.commit.run)
                });
            ensure!(issued, "Unknown operation ID");
            return Ok(self.view());
        };
        let now = now.max(state.now);
        if let Operation::Sleep { deadline, .. } = request {
            ensure!(now >= *deadline, "Timer completed before deadline");
        }
        self.usable = false;
        if self.live.is_none() {
            self.live = Some(self.restore(store, now).await?);
        }
        let operation = operation.to_owned();
        self.turn(store, Delivery::Complete { operation, outcome }, now)
            .await?;
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
        let golden = check(&self.programs, commit)?.memory();
        ensure!(
            commit.memory_len >= golden.len()
                && commit.memory_len <= MAX_MEMORY
                && commit.memory_len.is_multiple_of(65536),
            "Invalid committed memory size"
        );
        let mut memory = golden.to_vec();
        memory.resize(commit.memory_len, 0);
        for batch in &commit.chain {
            let bytes = store
                .get(&batch_key(*batch))
                .await?
                .context("Missing page batch")?;
            let (indexes, frame) = codec::page_indexes(&bytes)?;
            ensure!(
                indexes
                    .last()
                    .is_none_or(|&last| (last as usize + 1) * PAGE <= memory.len()),
                "Page beyond memory"
            );
            let decoded = codec::decode_pages(frame, &pages(&memory, &indexes))?;
            for (index, page) in indexes.iter().zip(decoded.chunks_exact(PAGE)) {
                let at = *index as usize * PAGE;
                memory[at..at + PAGE].copy_from_slice(page);
            }
        }
        let base = Image {
            meta: commit.meta.clone(),
            memory,
        };
        let mut vm = Vm::new(self.programs.runtime(), &commit.run, now)?;
        vm.restore(&base)?;
        Ok(Live { vm, base })
    }

    /// Run the JavaScript on `delivery`, capture what changed, and commit.
    async fn turn(&mut self, store: &impl Store, delivery: Delivery, now: u64) -> Result<()> {
        let golden = check(&self.programs, &self.commit)?.memory().clone();
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
                base.memory.resize(capture.memory_len, 0);
                let mut changed = Changed::default();
                for (index, new) in capture.pages {
                    let at = index as usize * PAGE;
                    let original = page(&golden, index as usize);
                    let old = &base.memory[at..at + PAGE];
                    live_pages =
                        live_pages + usize::from(new != original) - usize::from(old != original);
                    changed.indexes.push(index);
                    changed.old.extend_from_slice(old);
                    changed.new.extend_from_slice(&new);
                    base.memory[at..at + PAGE].copy_from_slice(&new);
                }
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
                let memory = &self.live.as_ref().context("No live VM")?.base.memory;
                let live: Vec<u32> = (0..(memory.len() / PAGE) as u32)
                    .filter(|&index| page(memory, index as usize) != page(&golden, index as usize))
                    .collect();
                bytes = codec::encode_pages(&live, &pages(memory, &live), &pages(&golden, &live))?;
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
            Some(live) => live.vm.committed(),
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
    match delivery {
        Delivery::Start(input) => {
            let seed = fingerprint(commit.run.as_bytes()) as u32;
            let init = json!({"runId": commit.run, "now": now, "maxTime": MAX_TIME, "seed": seed});
            vm.invoke("init", &init)?;
            vm.invoke("start", &input)?;
        }
        Delivery::Complete { operation, outcome } => {
            vm.invoke("deliver", &completion(&operation, outcome, now))?;
        }
    }
    // The requests already pending are the host's, issued in earlier turns.
    let mut seen: HashSet<String> = commit
        .state
        .pending
        .iter()
        .map(|op| op.id().into())
        .collect();
    let state = loop {
        vm.drain()?;
        let state = vm.inspect()?;
        let mut answered = false;
        for operation in &state.pending {
            if !seen.insert(operation.id().into()) {
                continue;
            }
            let value = match operation {
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
                _ => continue,
            };
            let outcome = Outcome::success(value);
            vm.invoke("deliver", &completion(operation.id(), outcome, now))?;
            answered = true;
        }
        if !answered {
            break state;
        }
        if vm.tired()? {
            // Every request so far is answered, and the answers are in the
            // heap and the job queue; no Wasm call is live. A wakeup, which
            // the host delivers in the next turn, runs the queued jobs then.
            vm.call("pause", None)?;
            let state = vm.inspect()?;
            wasmtime::ensure!(
                matches!(state.pending.last(), Some(Operation::Resume { .. })),
                "Missing resume wakeup"
            );
            break state;
        }
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

//! An object and its calls. An object is a program's module in one scope (a
//! key): its memory lives on from call to call. A run is one call of one of
//! its exported functions, turn by turn: a turn delivers one event to the
//! VM, runs the JavaScript until it waits, and commits.
//!
//! A scope's storage is a sqlite-kv database ([`Scope`]). A turn is one
//! commit of it, one put: the run's state ([`Commit`]) as the record's
//! effects, the pages the turn changed as a value of the record's chain, and
//! the turn's KV writes as SQL in its `kv` table. The image is the program's
//! golden image with the chain applied: the first batch holds the pages that
//! differ from golden, each next one the pages one turn changed, each
//! compressed against what it replaces, so a turn writes little more than
//! what it changed. Nothing is applied after a commit, and a crash before one
//! loses the whole turn, which runs again.
//!
//! The VM stays live from turn to turn of one attempt; a new attempt, or the
//! object's next call, restores it from the commit, once. A trap reverts the
//! turn: the memory to the last commit, and the turn's SQL with it.
use crate::{
    Scope, codec,
    db::Db,
    model::*,
    program::{Program, Programs},
    vm::{self, Vm},
};
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use serde_json::{Value, json};
use sqlite_kv::ChainUpdate;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;

/// Most batches in an image's chain. Restoring reads them all at once and
/// applies them in order; past this many, or once the later batches outweigh
/// twice the first, a turn writes every page that differs from golden into a
/// new first batch, so restoring stays bounded however long a run lives. The
/// size rule keeps rewriting to a third of what a run writes; this one only
/// bounds the reads, as each new first batch costs a whole image.
const MAX_CHAIN: usize = 256;

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

/// A call on an object, in one attempt. An error leaves it unusable: the
/// attempt starts again, from the store.
pub struct Run {
    programs: Arc<Programs>,
    /// The scope's database, this attempt's session of it, which the VM's
    /// host calls share.
    db: Arc<Mutex<Db>>,
    commit: Commit,
    /// The VM, and its memory as of the last commit: restored when a delivery
    /// needs it.
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
    /// The call's first turn: `handler(input)`, on the object `scope`.
    Start {
        scope: String,
        handler: String,
        input: Value,
    },
    /// Requests' outcomes, delivered in order before the JavaScript runs.
    Complete(Vec<(String, Outcome)>),
}

impl Run {
    /// Start the call `run` of `handler` on `input`, on the object of
    /// `program` whose key is `key` (its `ctx.key`), stored in `scope`: its
    /// first turn runs the handler until it waits or settles, and commits.
    /// The object's memory is as its last call left it. When the scope's
    /// commit is already this call's, this is another attempt at it: the call
    /// continues as committed, and `input` is the one it started with.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        programs: &Arc<Programs>,
        scope: &Scope,
        key: &str,
        run: &str,
        program: &str,
        handler: &str,
        input: Value,
        now: u64,
    ) -> Result<Self> {
        ensure!(
            !run.is_empty() && run.len() <= 1024 && !run.contains('/'),
            "Invalid run ID"
        );
        validate_time(now)?;
        input_size(&input)?;
        let db = Db::new(scope.open().await?);
        let previous = load(&db)?;
        let db = Arc::new(Mutex::new(db));
        if let Some(commit) = previous.as_ref().filter(|commit| commit.run == run) {
            ensure!(
                commit.program == program && commit.handler == handler,
                "Run {run} is a call of {}.{}",
                commit.program,
                commit.handler
            );
            check(programs, commit)?;
            return Ok(Self {
                programs: programs.clone(),
                db,
                commit: commit.clone(),
                live: None,
                last: Written::default(),
                usable: true,
            });
        }
        let state = State {
            pending: Vec::new(),
            next_operation: 0,
            now,
            status: Status::Waiting,
            outcome: None,
        };
        let (live, commit) = match previous {
            // The object's next call: on its memory, as committed. What its
            // last call left pending is dropped.
            Some(previous) => {
                ensure!(
                    previous.program == program,
                    "The object's memory is of program {}",
                    previous.program
                );
                check(programs, &previous)?;
                let live = restore(programs, &db, &previous, run, now).await?;
                let commit = Commit {
                    run: run.into(),
                    handler: handler.into(),
                    state,
                    ..previous
                };
                (live, commit)
            }
            None => {
                let golden = programs.get(program)?.image();
                let mut vm = Vm::new(programs.runtime(), run, now).await?;
                vm.restore(&golden).await?;
                vm.attach(db.clone());
                let commit = Commit {
                    run: run.into(),
                    handler: handler.into(),
                    program: program.into(),
                    version: programs.get(program)?.version(),
                    runtime: vm::VERSION,
                    turns: 0,
                    meta: golden.meta.clone(),
                    memory_len: golden.len(),
                    batches: 0,
                    base_bytes: 0,
                    delta_bytes: 0,
                    live_pages: 0,
                    state,
                };
                (Live { vm, base: golden }, commit)
            }
        };
        let mut this = Self {
            programs: programs.clone(),
            db,
            commit,
            live: Some(live),
            last: Written::default(),
            usable: false,
        };
        let start = Delivery::Start {
            scope: key.into(),
            handler: handler.into(),
            input,
        };
        this.turn(start, now).await?;
        this.usable = true;
        Ok(this)
    }

    /// Deliver the outcome of the pending request `operation` at `now`, and
    /// run a turn. A request delivered before, or dropped when the run
    /// settled, changes nothing.
    pub async fn deliver(&mut self, operation: &str, outcome: Outcome, now: u64) -> Result<View> {
        self.deliver_all(vec![(operation.to_owned(), outcome)], now)
            .await
    }

    /// Deliver the outcomes of several pending requests at `now`, in order,
    /// and run one turn: what arrived together commits once. Requests
    /// delivered before, or dropped when the run settled, are skipped.
    pub async fn deliver_all(
        &mut self,
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
            let live = restore(
                &self.programs,
                &self.db,
                &self.commit,
                &self.commit.run,
                now,
            );
            self.live = Some(live.await?);
        }
        self.turn(Delivery::Complete(pending), now).await?;
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

    /// Run the JavaScript on `delivery`, capture what changed, and commit.
    async fn turn(&mut self, delivery: Delivery, now: u64) -> Result<()> {
        let live = self.live.as_mut().context("No live VM")?;
        let ran = run(&mut live.vm, &self.db, &self.commit, delivery, now).await;
        let mut db = self.db.lock().await;
        let mut written = Written::default();
        let mut live_pages = self.commit.live_pages;
        let (state, meta, memory_len, changed) = match ran {
            Ok(state) => {
                let capture = live.vm.capture(&live.base).await?;
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
            // The guest trapped: its memory is mid-mutation, and the same
            // delivery would trap again. The call fails, and the turn is
            // undone: the object's memory is as last committed, and so is
            // its SQL.
            Err(error) => match vm::trap(&error) {
                Some(message) => {
                    self.live = None;
                    db.rollback_turn().await?;
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
        let (mut base_bytes, mut delta_bytes) = (previous.base_bytes, previous.delta_bytes);
        let mut batches = previous.batches;
        let update = if changed.indexes.is_empty() {
            None
        } else if batches == 0 {
            // The object's first batch, against golden.
            let bytes = codec::encode_pages(&changed.indexes, &changed.new, &changed.old)?;
            (base_bytes, delta_bytes, batches) = (bytes.len(), 0, 1);
            written.batch_bytes = bytes.len();
            Some(ChainUpdate::Restart(bytes.into()))
        } else {
            let bytes = codec::encode_pages(&changed.indexes, &changed.new, &changed.old)?;
            if batches >= MAX_CHAIN || delta_bytes + bytes.len() > 2 * base_bytes {
                // A new first batch: every page that differs from golden.
                let base = &self.live.as_ref().context("No live VM")?.base;
                let live: Vec<u32> = base.changed().collect();
                let bytes = codec::encode_pages(
                    &live,
                    &pages(|i| base.page(i), &live),
                    &pages(|i| base.original(i), &live),
                )?;
                (base_bytes, delta_bytes, batches) = (bytes.len(), 0, 1);
                written.batch_bytes = bytes.len();
                Some(ChainUpdate::Restart(bytes.into()))
            } else {
                delta_bytes += bytes.len();
                batches += 1;
                written.batch_bytes = bytes.len();
                Some(ChainUpdate::Append(bytes.into()))
            }
        };
        let commit = Commit {
            run: previous.run.clone(),
            handler: previous.handler.clone(),
            program: previous.program.clone(),
            version: previous.version,
            runtime: previous.runtime,
            turns: previous.turns + 1,
            meta,
            memory_len,
            batches,
            base_bytes,
            delta_bytes,
            live_pages,
            state,
        };
        let effects = Bytes::from(json_bytes(&commit)?);
        written.commit_bytes = effects.len();
        db.commit_turn(effects, update).await?;
        drop(db);
        self.commit = commit;
        self.last = written;
        if let Some(live) = &mut self.live {
            live.vm.committed()?;
        }
        Ok(())
    }
}

/// The committed image, from golden and the batches, in a new VM.
async fn restore(
    programs: &Programs,
    db: &Arc<Mutex<Db>>,
    commit: &Commit,
    identity: &str,
    now: u64,
) -> Result<Live> {
    let mut base = check(programs, commit)?.image();
    ensure!(
        commit.memory_len >= base.len()
            && commit.memory_len <= MAX_MEMORY
            && commit.memory_len.is_multiple_of(65536),
        "Invalid committed memory size"
    );
    base.meta = commit.meta.clone();
    base.grow(commit.memory_len);
    // Read with the record at open: the chain is the object's.
    let batches = db.lock().await.session().chain();
    ensure!(batches.len() == commit.batches, "Missing page batch");
    for bytes in batches {
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
    let mut vm = Vm::new(programs.runtime(), identity, now).await?;
    vm.restore(&base).await?;
    vm.attach(db.clone());
    Ok(Live { vm, base })
}

/// Deliver to the VM, and run the JavaScript until it waits on what the host
/// answers (RPCs, timers, wakeups), settles, or spends half its fuel. KV
/// requests are answered here, in the turn's SQL transaction: a read sees the
/// turn's own writes, and its SQL statements'.
async fn run(
    vm: &mut Vm,
    db: &Mutex<Db>,
    commit: &Commit,
    delivery: Delivery,
    now: u64,
) -> wasmtime::Result<State> {
    vm.begin_turn(now)?;
    // The host keeps the requests pending: those of earlier turns, less the
    // ones delivered, then those the guest issues, as it reports them.
    let mut pending = commit.state.pending.clone();
    match delivery {
        Delivery::Start {
            scope,
            handler,
            input,
        } => {
            // The object's generator is seeded once, from its key.
            let seed = fingerprint(scope.as_bytes()) as u32;
            let init = json!({"scope": scope, "maxTime": MAX_TIME, "seed": seed});
            vm.invoke("init", &init).await?;
            let call = json!({"runId": commit.run, "handler": handler, "input": input, "now": now});
            vm.invoke("start", &call).await?;
        }
        Delivery::Complete(outcomes) => {
            for (operation, outcome) in outcomes {
                pending.retain(|pending| pending.id() != operation);
                vm.invoke("deliver", &completion(&operation, outcome, now))
                    .await?;
            }
        }
    }
    let report = loop {
        vm.drain().await?;
        let report = vm.inspect().await?;
        let mut answered = false;
        for operation in report.issued.iter().cloned() {
            let kv = match &operation {
                Operation::KvGet { key, .. } => db.lock().await.kv_read(key).await,
                Operation::KvSet { key, value, .. } => {
                    db.lock().await.kv_write(key, Some(value)).await
                }
                Operation::KvDelete { key, .. } => db.lock().await.kv_write(key, None).await,
                _ => {
                    pending.push(operation);
                    continue;
                }
            };
            let value = kv.map_err(wasmtime::Error::from_anyhow)?;
            let outcome = Outcome::success(value);
            vm.invoke("deliver", &completion(operation.id(), outcome, now))
                .await?;
            answered = true;
        }
        if !answered {
            break report;
        }
        if vm.tired()? {
            // Every request so far is answered, and the answers are in the
            // heap and the job queue; no Wasm call is live. A wakeup, which
            // the host delivers in the next turn, runs the queued jobs then.
            vm.call("pause", None).await?;
            let report = vm.inspect().await?;
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

/// The object, as its database's last commit holds it: its memory, and its
/// last call.
fn load(db: &Db) -> Result<Option<Commit>> {
    let effects = db.session().effects();
    if effects.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&effects)?))
}

/// The object's program, as the version and runtime its memory is of.
fn check<'a>(programs: &'a Programs, commit: &Commit) -> Result<&'a Program> {
    let program = programs.get(&commit.program)?;
    ensure!(
        program.version() == commit.version,
        "The object's memory is of another version of program {}",
        commit.program
    );
    ensure!(
        commit.runtime == vm::VERSION,
        "The object's memory is of another version of the runtime"
    );
    Ok(program)
}

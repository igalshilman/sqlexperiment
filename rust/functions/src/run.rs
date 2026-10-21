//! A run: one call of a function a program exports, turn by turn. Each run
//! starts from the program's golden image: its memory lasts for the run. A turn delivers
//! one event to the VM, runs the JavaScript until it waits, and seals.
//!
//! A turn is one transaction (see [`crate::transaction`]): its participants
//! are the run's state ([`RunState`], the record "run"), its memory
//! ([`Memory`], the record "memory" and the pages the turn changed, in the
//! scope's file over the program's golden memory), the scope's tables (the
//! turn's SQL, KV included), and each extension. The host seals it in one
//! put, the scope's database's commit. The image is the program's golden
//! image with the file's pages that differ from it; the journal compresses
//! each against what it replaces (golden's, at first), so a turn writes
//! little more than what it changed. Nothing is applied after a seal, and a
//! crash before one loses the whole turn, which runs again.
//!
//! The VM stays live from turn to turn of one attempt; a new attempt restores
//! it from the seal, once, when a delivery needs it.
use crate::{
    Scope,
    db::Db,
    extension::{Attachments, HostCall, Making, Tables},
    model::*,
    program::{Program, Programs},
    transaction::{Participant, Record, ScopeBlobs},
    vm::{self, Vm},
};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use journal::{Base, File, Journal};
use serde_json::{Value, json};
use sql::Sql;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;

fn input_size<T: serde::Serialize>(input: &T) -> Result<()> {
    ensure!(json_bytes(input)?.len() <= MAX_INPUT, "Input exceeds limit");
    Ok(())
}

/// The scope's file a program's runs keep their memory in: one per golden
/// memory, over it.
fn memory_file(golden: &crate::cow::Golden) -> String {
    format!("memory/{:016x}", golden.id())
}

/// A run of a program in one scope, in one attempt. An error leaves it
/// unusable: the attempt starts again, from the store.
pub struct Run {
    programs: Arc<Programs>,
    /// The scope's journal, which each turn commits.
    journal: Journal,
    /// The scope's database, this attempt's session of it, which the VM's
    /// host calls share.
    db: Arc<Mutex<Db>>,
    /// The extensions' parts of this attempt, which the VM's host calls share.
    extensions: Arc<Mutex<Attachments>>,
    /// The scope's blobs, which the host collects after each seal.
    blobs: ScopeBlobs,
    state: RunState,
    memory: Memory,
    last: Written,
    usable: bool,
}

/// The run's state, a participant: the record "run".
struct RunState {
    record: RunRecord,
    sealed: RunRecord,
    stored: Record,
}

impl RunState {
    fn new(record: RunRecord, stored: Record) -> Self {
        Self {
            sealed: record.clone(),
            record,
            stored,
        }
    }
}

#[async_trait]
impl Participant for RunState {
    async fn prepare(&mut self) -> Result<()> {
        self.stored.set(json_bytes(&self.record)?.into());
        Ok(())
    }

    fn committed(&mut self) {
        self.sealed = self.record.clone();
    }

    fn aborted(&mut self) {
        self.record = self.sealed.clone();
    }
}

/// The run's memory, a participant: the record "memory", and the pages each
/// turn changed, in the scope's file over the program's golden memory.
struct Memory {
    record: MemoryRecord,
    stored: Record,
    /// The file over golden.
    file: File,
    /// The VM, and its memory as of the last seal: restored when a delivery
    /// needs it, and dropped once the run has settled.
    live: Option<Live>,
    /// What this turn changed, to seal.
    turn: Option<Turn>,
    /// The scope's previous run's pages, in its file, which this run's first
    /// seal makes holes: they are not this run's.
    stale: Option<(File, Vec<u32>)>,
}

/// What a turn did to the memory: the pages it changed.
struct Turn {
    changed: Vec<u32>,
    settled: bool,
}

struct Live {
    vm: Vm,
    base: Image,
}

impl Memory {
    /// The turn captured `capture`, as `live`'s new image.
    fn captured(&mut self, capture: Capture, settled: bool) -> Result<()> {
        let live = self.live.as_mut().context("No live VM")?;
        let base = &mut live.base;
        base.meta = capture.meta.clone();
        base.grow(capture.memory_len);
        let mut changed = Vec::with_capacity(capture.pages.len());
        for (index, new) in capture.pages {
            changed.push(index);
            base.set(index, &new);
        }
        self.record.meta = capture.meta;
        self.record.memory_len = capture.memory_len;
        self.turn = Some(Turn { changed, settled });
        Ok(())
    }

    /// The guest trapped: its image is unusable, and the run ends, keeping
    /// none.
    fn trapped(&mut self) {
        self.live = None;
        self.turn = Some(Turn {
            changed: Vec::new(),
            settled: true,
        });
    }
}

#[async_trait]
impl Participant for Memory {
    /// The pages the turn changed, into the file (the journal writes one
    /// that is golden's again as a hole); a settled run's, as holes.
    async fn prepare(&mut self) -> Result<()> {
        let Turn { changed, settled } = self.turn.take().context("No turn to seal")?;
        if let Some((file, pages)) = self.stale.take() {
            for page in pages {
                file.write(page.into(), None).map_err(sql)?;
            }
        }
        match (&self.live, settled) {
            // A settled run never restores again: it keeps its outcome, not
            // its image.
            (_, true) => {
                for page in self.record.pages().chain(changed) {
                    self.file.write(page.into(), None).map_err(sql)?;
                }
                self.record.pages.clear();
                self.live = None;
            }
            (Some(live), false) => {
                for page in changed {
                    let image = bytes::Bytes::copy_from_slice(live.base.page(page));
                    self.file.write(page.into(), Some(image)).map_err(sql)?;
                }
                self.record.set_pages(live.base.changed());
            }
            (None, false) => anyhow::bail!("No live VM"),
        }
        self.stored.set(json_bytes(&self.record)?.into());
        Ok(())
    }

    /// The image no longer matches a seal: the attempt is over.
    fn aborted(&mut self) {
        self.live = None;
    }
}

fn sql(error: Box<dyn std::error::Error + Send + Sync>) -> anyhow::Error {
    anyhow::anyhow!(error)
}

/// What a turn wrote.
#[derive(Default)]
struct Written {
    changed_pages: usize,
    examined_pages: usize,
    logged_bytes: usize,
    commit_bytes: usize,
}

enum Delivery {
    /// The run's first turn: `handler(input)`, in `scope`.
    Start {
        scope: String,
        handler: String,
        input: Value,
    },
    /// Requests' outcomes, delivered in order before the JavaScript runs.
    Complete(Vec<(String, Outcome)>),
}

impl Run {
    /// Start run `run` of `program`'s `handler` on `input`, in the scope `key`
    /// (the program's `ctx.key`), stored in `scope`: its first turn runs the
    /// handler until it waits or settles, and seals. When the scope's seal is
    /// already this run's, this is another attempt at it: the run continues as
    /// sealed, and `input` is the one it started with.
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
        let journal = scope.open().await?;
        let db = Db::new(Sql::open(&journal).await.map_err(|error| anyhow!(error))?);
        let [stored_run, stored_memory] = BUILT_IN_RECORDS.map(|name| journal.record(name));
        // What the scope's last seal kept of each extension, whichever run
        // made it. The scope is the program's object key, as its journal
        // is: another program's objects of the same key keep their own.
        let blobs = ScopeBlobs::new(scope.extensions.blobs.clone(), &format!("{program}/{key}"));
        let extensions = Attachments::attach(&scope.extensions, &blobs, &db).await?;
        let sealed: Option<RunRecord> = json_record(&stored_run)?;
        let sealed_memory: Option<MemoryRecord> = json_record(&stored_memory)?;
        if let Some(record) = sealed.filter(|record| record.run == run) {
            ensure!(
                record.program == program && record.handler == handler,
                "Run {run} is a call of {}.{}",
                record.program,
                record.handler
            );
            let golden = check(programs, &record)?.image();
            let memory = sealed_memory.context("No memory in the seal")?;
            let golden = golden.golden_memory();
            ensure!(
                memory.file == memory_file(golden),
                "The memory of another image"
            );
            let file = journal
                .file_with_base(&memory.file, golden.clone() as Arc<dyn Base>)
                .map_err(sql)?;
            return Ok(Self {
                programs: programs.clone(),
                journal,
                db: Arc::new(Mutex::new(db)),
                extensions: Arc::new(Mutex::new(extensions)),
                blobs,
                state: RunState::new(record, stored_run),
                memory: Memory {
                    record: memory,
                    stored: stored_memory,
                    file,
                    live: None,
                    turn: None,
                    stale: None,
                },
                last: Written::default(),
                usable: true,
            });
        }
        // A new run: the scope's previous run's pages, if any, go with its
        // first seal.
        let stale = match sealed_memory {
            Some(old) if !old.pages.is_empty() => {
                let file = journal.file(&old.file).map_err(sql)?;
                Some((file, old.pages().collect()))
            }
            _ => None,
        };
        let golden = programs.get(program)?.image();
        let name = memory_file(golden.golden_memory());
        let file = journal
            .file_with_base(&name, golden.golden_memory().clone() as Arc<dyn Base>)
            .map_err(sql)?;
        let db = Arc::new(Mutex::new(db));
        let extensions = Arc::new(Mutex::new(extensions));
        let language = programs.get(program)?.language();
        let mut vm = Vm::for_image(programs.runtime(), language, &golden, run, now).await?;
        vm.restore(&golden).await?;
        vm.attach(db.clone(), extensions.clone());
        let record = RunRecord {
            run: run.into(),
            handler: handler.into(),
            program: program.into(),
            version: programs.get(program)?.version(),
            runtime: vm::VERSION,
            turns: 0,
            state: State {
                pending: Vec::new(),
                next_operation: 0,
                now,
                status: Status::Waiting,
                outcome: None,
            },
        };
        let memory = Memory {
            record: MemoryRecord {
                meta: golden.meta.clone(),
                memory_len: golden.len(),
                file: name,
                pages: Vec::new(),
            },
            stored: stored_memory,
            file,
            live: Some(Live { vm, base: golden }),
            turn: None,
            stale,
        };
        let mut this = Self {
            programs: programs.clone(),
            journal,
            db,
            extensions,
            blobs,
            state: RunState::new(record, stored_run),
            memory,
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
    /// and run one turn: what arrived together seals once. Requests
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
        let record = &self.state.record;
        let state = &record.state;
        let now = now.max(state.now);
        let mut delivered = HashSet::new();
        let mut pending = Vec::new();
        for (operation, outcome) in outcomes {
            outcome.validate()?;
            input_size(&outcome)?;
            ensure!(delivered.insert(operation.clone()), "Duplicate outcome");
            let Some(request) = state.pending.iter().find(|p| p.id() == operation) else {
                let issued = operation
                    .strip_prefix(&format!("{}/", record.run))
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| {
                        n < state.next_operation && operation == format!("{}/{n}", record.run)
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
        if self.memory.live.is_none() {
            self.memory.live = Some(self.restore(now).await?);
        }
        self.turn(Delivery::Complete(pending), now).await?;
        self.usable = true;
        Ok(self.view())
    }

    /// The memory the run holds while live: its VM's, and the image beside
    /// it (each page that differs from golden). Parked, it holds none.
    pub fn resident(&self) -> usize {
        match self.memory.live {
            Some(_) => self.footprint(),
            None => 0,
        }
    }

    /// The memory the run will hold once its next delivery restores it.
    pub fn footprint(&self) -> usize {
        let memory = &self.memory.record;
        memory.memory_len + memory.live_pages() * PAGE
    }

    /// The memory the last turn held at its peak: the run's, and the seal's
    /// buffers (the pages it changed, captured, before and after, and
    /// encoded).
    pub fn turn_peak(&self) -> usize {
        let (last, memory) = (&self.last, &self.memory.record);
        memory.memory_len
            + memory.live_pages().max(last.changed_pages) * PAGE
            + 3 * last.changed_pages * PAGE
            + last.logged_bytes
            + last.commit_bytes
    }

    /// Let go of the VM and its image: the next delivery restores them from
    /// the scope, as a retried attempt would. Only between turns, which have
    /// sealed all the run is.
    pub fn park(&mut self) {
        self.memory.live = None;
        // Nothing holds the extensions between turns.
        if let Ok(mut extensions) = self.extensions.try_lock() {
            extensions.park();
        }
    }

    /// The input of the pending call `operation` (an RPC, or a binding's
    /// call), read from the guest's heap, where it stays until the call is
    /// answered: the host reads it when it makes the call, and again on
    /// another attempt. A new attempt restores the VM to read it. The inner
    /// `Err` is the guest's: its input is not JSON, and the call fails.
    pub async fn input(&mut self, operation: &str) -> Result<Result<Value, GuestError>> {
        ensure!(
            self.usable,
            "This attempt at the run failed: start it again"
        );
        let state = &self.state.record.state;
        ensure!(
            (state.pending.iter())
                .filter(|p| matches!(p, Operation::Rpc { .. } | Operation::Call { .. }))
                .any(|p| p.id() == operation),
            "No pending call {operation}"
        );
        let now = state.now;
        self.usable = false;
        if self.memory.live.is_none() {
            self.memory.live = Some(self.restore(now).await?);
        }
        let live = self.memory.live.as_mut().context("No live VM")?;
        live.vm.begin_turn(now)?;
        let answer = match live.vm.call("input", Some(operation)).await {
            Ok(answer) => answer,
            // The guest ran out of memory or time serializing it: its image is
            // unusable, and would fail the same way again.
            Err(error) => match vm::trap(&error) {
                Some(message) => {
                    self.memory.live = None;
                    self.usable = true;
                    let message = format!("The input could not be read: {message}");
                    return Ok(Err(GuestError::new("RangeError", message)));
                }
                None => return Err(error.into()),
            },
        };
        self.usable = true;
        Ok(match serde_json::from_str(&answer)? {
            Outcome::Success { value, .. } => Ok(value),
            Outcome::Failure { error, .. } => Err(error),
        })
    }

    /// Make the pending operation `operation`, an extension's: its input
    /// read from the heap, and what the extension reads of the seal. The
    /// inner `Err` fails the operation, as the guest sees it.
    pub async fn operation(&mut self, operation: &str) -> Result<Result<Making, GuestError>> {
        let pending = self.state.record.state.pending.iter();
        let call = pending.clone().find(|p| p.id() == operation).cloned();
        let Some(call @ Operation::Call { .. }) = call else {
            anyhow::bail!("No pending operation {operation}");
        };
        let Operation::Call { id, name, .. } = &call else {
            unreachable!()
        };
        let mut extensions = self.extensions.lock().await;
        let Some(extension) = extensions.get(name) else {
            return Ok(Err(GuestError::new(
                "Error",
                format!("No extension {name:?}"),
            )));
        };
        if call.idempotency_key() != id && !extension.deduplicates() {
            let message = format!("{name} does not deduplicate: it takes no idempotency key");
            return Ok(Err(GuestError::new("TypeError", message)));
        }
        drop(extensions);
        let input = match self.input(operation).await? {
            Ok(input) => input,
            Err(error) => return Ok(Err(error)),
        };
        let host_call = HostCall {
            id: id.clone(),
            name: name.clone(),
            idempotency_key: call.idempotency_key().to_owned(),
            input,
        };
        let mut extensions = self.extensions.lock().await;
        let extension = extensions.get(name).context("No extension")?;
        let mut db = self.db.lock().await;
        let mut tables = Tables { db: &mut db };
        Ok(extension.operation(host_call, &mut tables).await)
    }

    /// The run as of its last seal.
    pub fn view(&self) -> View {
        let (record, memory) = (&self.state.record, &self.memory.record);
        View {
            run: record.run.clone(),
            program: record.program.clone(),
            turns: record.turns,
            state: record.state.clone(),
            memory_bytes: memory.memory_len,
            live_pages: memory.live_pages(),
            changed_pages: self.last.changed_pages,
            examined_pages: self.last.examined_pages,
            logged_bytes: self.last.logged_bytes,
            commit_bytes: self.last.commit_bytes,
        }
    }

    /// The sealed image, from golden and the batches, in a new VM.
    async fn restore(&self, now: u64) -> Result<Live> {
        let (record, memory) = (&self.state.record, &self.memory.record);
        let mut base = check(&self.programs, record)?.image();
        ensure!(
            memory.memory_len >= base.len()
                && memory.memory_len <= MAX_MEMORY
                && memory.memory_len.is_multiple_of(65536),
            "Invalid sealed memory size"
        );
        base.meta = memory.meta.clone();
        base.grow(memory.memory_len);
        // The pages that differ from golden, read together: from the log,
        // the cache, or the store.
        let indexes: Vec<u32> = memory.pages().collect();
        ensure!(
            (indexes.last()).is_none_or(|&last| (last as usize + 1) * PAGE <= base.len()),
            "Page beyond memory"
        );
        let file = &self.memory.file;
        let reads = indexes.iter().map(|&index| file.read(index.into()));
        let pages = futures::future::try_join_all(reads).await.map_err(sql)?;
        for (index, page) in indexes.iter().zip(pages) {
            static ZERO: [u8; PAGE] = [0; PAGE];
            base.set(*index, page.as_deref().unwrap_or(&ZERO));
        }
        let language = self.programs.get(&record.program)?.language();
        let mut vm =
            Vm::for_image(self.programs.runtime(), language, &base, &record.run, now).await?;
        vm.restore(&base).await?;
        vm.attach(self.db.clone(), self.extensions.clone());
        Ok(Live { vm, base })
    }

    /// Run the JavaScript on `delivery`, capture what changed, and seal.
    async fn turn(&mut self, delivery: Delivery, now: u64) -> Result<()> {
        let live = self.memory.live.as_mut().context("No live VM")?;
        let record = &self.state.record;
        let ran = match run(
            &mut live.vm,
            &self.db,
            &self.extensions,
            record,
            delivery,
            now,
        )
        .await
        {
            Ok(state) => live
                .vm
                .capture(&live.base)
                .await
                .map(|capture| (state, capture)),
            Err(error) => Err(error),
        };
        let mut written = Written::default();
        let state = match ran {
            Ok((state, capture)) => {
                written.changed_pages = capture.pages.len();
                written.examined_pages = capture.examined_pages;
                let settled = state.status != Status::Waiting;
                self.memory.captured(capture, settled)?;
                state
            }
            // The guest trapped: its image is unusable, and the same delivery
            // would trap again. The run ends; the KV writes before the trap
            // stand, as for a thrown error.
            Err(error) => match vm::trap(&error) {
                Some(message) => {
                    self.memory.trapped();
                    self.db.lock().await.abandon_transactions().await?;
                    State {
                        pending: Vec::new(),
                        next_operation: record.state.next_operation,
                        now,
                        status: Status::Failed,
                        outcome: Some(Outcome::failure("Trap", message)),
                    }
                }
                None => return Err(error.into()),
            },
        };
        if state.status != Status::Waiting {
            self.extensions.lock().await.ended();
        }
        self.state.record.state = state;
        self.state.record.turns += 1;

        // The transaction: each participant sets its records, the host
        // writes the seal, and tells them.
        let mut extensions = self.extensions.lock().await;
        let mut db = self.db.lock().await;
        let mut participants: Vec<&mut dyn Participant> =
            vec![&mut self.state, &mut self.memory, &mut *db];
        let mut prepared = extensions.prepare().await;
        for participant in &mut participants {
            if prepared.is_err() {
                break;
            }
            prepared = participant.prepare().await;
        }
        drop(participants);
        let sealed = match prepared {
            Ok(()) => (self.journal.commit().await)
                .map(|report| {
                    written.logged_bytes = report.logged_bytes;
                    written.commit_bytes = report.record_bytes;
                })
                .map_err(|error| anyhow!(error)),
            Err(error) => Err(error),
        };
        let mut participants: Vec<&mut dyn Participant> =
            vec![&mut self.state, &mut self.memory, &mut *db];
        if let Err(error) = sealed {
            participants.iter_mut().for_each(|p| p.aborted());
            extensions.aborted();
            return Err(error);
        }
        participants.iter_mut().for_each(|p| p.committed());
        extensions.committed();
        drop(db);
        self.last = written;
        // After it: the blobs no seal names any more go.
        if let Err(error) = self.blobs.collect(&extensions.holds()).await {
            tracing::warn!("Collecting blobs failed: {error:#}");
        }
        drop(extensions);
        // The turn is sealed: a VM that cannot follow is dropped, and the
        // next turn restores the run from the seal.
        if let Some(Err(error)) = self.memory.live.as_mut().map(|live| live.vm.committed()) {
            tracing::warn!("Dropping the live VM: {error:#}");
            self.memory.live = None;
        }
        Ok(())
    }
}

/// Deliver to the VM, and run the JavaScript until it waits on what the host
/// answers (RPCs, timers, wakeups), settles, or spends half its fuel. KV
/// requests are answered here, in the turn's SQL transaction: a read sees the
/// turn's own writes, and its SQL statements'.
async fn run(
    vm: &mut Vm,
    db: &Mutex<Db>,
    extensions: &Mutex<Attachments>,
    commit: &RunRecord,
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
            let seed = fingerprint(commit.run.as_bytes()) as u32;
            let init = json!({
                "runId": commit.run,
                "scope": scope,
                "now": now,
                "maxTime": MAX_TIME,
                "seed": seed,
            });
            vm.invoke("init", &init).await?;
            vm.invoke("start", &json!({"handler": handler, "input": input}))
                .await?;
        }
        Delivery::Complete(outcomes) => {
            for (operation, mut outcome) in outcomes {
                // An extension's operation: it keeps what the outcome
                // brought, in this turn, before the guest sees it.
                if let Some(call @ Operation::Call { name, .. }) =
                    pending.iter().find(|pending| pending.id() == operation)
                {
                    let mut extensions = extensions.lock().await;
                    if let Some(extension) = extensions.get(name) {
                        let mut db = db.lock().await;
                        let mut tables = Tables { db: &mut db };
                        (extension.delivered(&mut tables, call, &mut outcome).await)
                            .map_err(wasmtime::Error::from_anyhow)?;
                    }
                }
                pending.retain(|pending| pending.id() != operation);
                vm.invoke("deliver", &completion(&operation, outcome, now))
                    .await?;
            }
        }
    }
    // Operation IDs are the run's and numbered in order: an ID, and the
    // idempotency key made from it, names one request on every attempt.
    let mut next = commit.state.next_operation;
    let mut numbered = |operation: &Operation| {
        let expected = format!("{}/{next}", commit.run);
        next += 1;
        wasmtime::ensure!(operation.id() == expected, "Unexpected operation ID");
        Ok(())
    };
    let report = loop {
        vm.drain().await?;
        let report = vm.inspect().await?;
        report.issued.iter().try_for_each(&mut numbered)?;
        let mut answered = false;
        for operation in report.issued.iter().cloned() {
            let kv = match &operation {
                Operation::KvGet { key, .. } => db.lock().await.kv_read(key).await.map(Ok),
                Operation::KvSet { key, value, .. } => {
                    db.lock().await.kv_write(key, Some(value)).await.map(Ok)
                }
                Operation::KvDelete { key, .. } => {
                    db.lock().await.kv_write(key, None).await.map(Ok)
                }
                // An extension's operation, which it may refuse now.
                Operation::Call { name, .. } => {
                    let refused = match extensions.lock().await.get(name) {
                        Some(extension) => extension.issued(&operation).err(),
                        None => None,
                    };
                    match refused {
                        Some(error) => Ok(Err(error)),
                        None => {
                            pending.push(operation);
                            continue;
                        }
                    }
                }
                _ => {
                    pending.push(operation);
                    continue;
                }
            };
            let outcome = match kv.map_err(wasmtime::Error::from_anyhow)? {
                Ok(value) => Outcome::success(value),
                Err(error) => Outcome::failure(error.name, error.message),
            };
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
            report.issued.iter().try_for_each(&mut numbered)?;
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

/// The records of the run's state and memory.
const BUILT_IN_RECORDS: [&str; 2] = ["run", "memory"];

/// A record's JSON value, as the last seal left it; `None` if none did.
fn json_record<T: serde::de::DeserializeOwned>(record: &Record) -> Result<Option<T>> {
    let bytes = record.get();
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// The run's program, as the version and runtime it started on.
fn check<'a>(programs: &'a Programs, commit: &RunRecord) -> Result<&'a Program> {
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

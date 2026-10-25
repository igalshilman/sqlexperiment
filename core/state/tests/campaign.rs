//! A seeded random campaign over the public API: mutations of changes to several
//! files (one with a base), records and logs, flushed with small chunks and random
//! rebase ratios so that deltas, rebases and garbage deletion all happen,
//! and random faults, cancellations, crashes and reopens by owners with and
//! without a cache, every request checked by the protocol monitor. After
//! each reopen the state reads as the model: every acknowledged flush, and
//! an unacknowledged one wholly or not at all. Readers opened beside the
//! owner read the state they opened while it flushes.
//!
//! ```sh
//! cargo test --features simulation --test campaign                  # a few seeds
//! STATE_SEEDS=1000 cargo test --release --features simulation --test campaign
//! STATE_SEED=17 cargo test --features simulation --test campaign  # one, as a failure prints it
//! ```
#![cfg(feature = "simulation")]
mod support;

use futures::task::noop_waker_ref;
use state::{FlushReport, Mutation, PageCache, Result, StateOptions, kv::KvStore, simulation::*};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
};
use support::*;

/// Seeds `cargo test` runs without `STATE_SEEDS`.
const SEEDS: u64 = 12;
/// Flushes per seed.
const STEPS: usize = 40;

type Store = MonitorKv<FaultKv>;

/// The seeds to run: `STATE_SEED`, or the first `STATE_SEEDS`.
fn seeds() -> Vec<u64> {
    let var = |name| {
        std::env::var(name)
            .ok()
            .map(|v: String| v.parse().expect(name))
    };
    match (var("STATE_SEED"), var("STATE_SEEDS")) {
        (Some(seed), _) => vec![seed],
        (None, count) => (0..count.unwrap_or(SEEDS)).collect(),
    }
}

#[test]
fn random_faults_never_lose_an_acknowledged_flush_or_split_one() {
    let mut totals = Totals::default();
    for seed in seeds() {
        let run = std::panic::catch_unwind(|| futures::executor::block_on(run(seed)));
        let rerun = format!(
            "STATE_SEED={seed} cargo test --features simulation --test campaign -- --nocapture"
        );
        match run {
            Ok(Ok(run)) => totals.add(&run),
            Ok(Err(error)) => panic!("seed {seed}: {error}\nrerun: {rerun}"),
            Err(panic) => {
                eprintln!("seed {seed} panicked; rerun: {rerun}");
                std::panic::resume_unwind(panic)
            }
        }
    }
    println!("{totals:?}");
    if seeds().len() as u64 >= SEEDS {
        let t = &totals;
        assert!(
            t.blobs > 0 && t.collected > 0 && t.sweeps > 0 && t.rebased > 0 && t.reads_beside > 0,
            "{t:?}"
        );
        assert!(
            t.landed > 0 && t.lost > 0 && t.crashes > 0 && t.cancelled > 0,
            "{t:?}"
        );
    }
}

/// What a run did, to check that the campaign covers what it should.
#[derive(Debug, Default)]
struct Totals {
    flushes: usize,
    /// Unacknowledged flushes found whole after a reopen, and found absent.
    landed: usize,
    lost: usize,
    crashes: usize,
    cancelled: usize,
    handovers: usize,
    requests_checked: usize,
    /// Blobs put, and deleted as garbage; chunks rebased.
    blobs: u64,
    collected: u64,
    rebased: usize,
    /// Reads by readers beside the owner, after it flushed.
    reads_beside: usize,
    /// Sweeps by a flush, the owner or a worker beside it, faulty ones
    /// included.
    sweeps: usize,
}

impl Totals {
    fn add(&mut self, run: &Totals) {
        self.flushes += run.flushes;
        self.landed += run.landed;
        self.lost += run.lost;
        self.crashes += run.crashes;
        self.cancelled += run.cancelled;
        self.handovers += run.handovers;
        self.requests_checked += run.requests_checked;
        self.blobs += run.blobs;
        self.collected += run.collected;
        self.rebased += run.rebased;
        self.reads_beside += run.reads_beside;
        self.sweeps += run.sweeps;
    }
}

/// A cache too small to keep much, for some owners.
fn small_cache() -> &'static PageCache {
    static CACHE: OnceLock<PageCache> = OnceLock::new();
    CACHE.get_or_init(|| open_cache(cache_config(64 << 10, 1 << 20)))
}

/// A new owner process's cache: its first look at the namespace.
fn new_owner(rng: &mut Rng) -> Namespace {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    match rng.percent(30) {
        true => Namespace::new(
            small_cache(),
            &format!("campaign-{}", KEYS.fetch_add(1, Relaxed)),
        ),
        false => fresh_namespace(),
    }
}

/// One seed's world: the store, the owners, and what the state may hold.
struct World {
    rng: Rng,
    store: Arc<Store>,
    memory: MemoryKv,
    /// Two owner processes, each with its own cache.
    caches: [Namespace; 2],
    /// The owner of the namespace: a process, or one without a cache.
    owner: Option<usize>,
    /// The state as of the last acknowledged flush or reopen.
    model: Model,
    /// The states a crash may go back to: the durable one, then each one
    /// read or flushed since the last sync.
    since_sync: Vec<Model>,
    /// Flushes after the last root that named a blob before it goes: the
    /// world's, as readers rely on it.
    garbage_after: u64,
    /// Readers beside the owner, and what they opened.
    readers: Vec<(Session, Model)>,
    totals: Totals,
}

impl World {
    /// Random options: they never change the stored format.
    fn options(&mut self) -> Setup {
        let cache = self.owner.map(|owner| self.caches[owner].clone());
        let rng = &mut self.rng;
        Setup {
            options: StateOptions {
                // Kept by the namespace's first root.
                chunk_pages: rng.pick(&[1, 2, 4, 16]),
                fanout: rng.pick(&[2, 3, 4, 64]),
                rebase_ratio: rng.pick(&[0.0, 0.25, 1.0, 100.0]),
                inline_record_bytes: rng.pick(&[0, 200, 1024, 8192]),
                prefetch_pages: rng.pick(&[0, 4, 16]),
                garbage_after: self.garbage_after,
                sweep_every: rng.pick(&[0, 1, 3, 16]),
            },
            cache,
        }
    }

    fn violations(&self) -> std::result::Result<(), String> {
        match self.store.violations() {
            violations if violations.is_empty() => Ok(()),
            violations => Err(format!("protocol violations: {violations:#?}")),
        }
    }

    /// The writes since the last sync become durable, or a crash keeps a
    /// prefix of them: an owner's requests settle before another owns the
    /// namespace.
    async fn settle(&mut self) -> Result<()> {
        if self.rng.percent(50) {
            self.crash();
        } else {
            self.memory.sync().await?;
        }
        Ok(())
    }

    /// A crash keeps a random prefix of the writes since the last sync.
    fn crash(&mut self) {
        let writes = self.memory.unsynced().len() as u64;
        let kept = self.rng.below(writes + 1) as usize;
        trace(|| format!("crash keeping {kept} of {writes} writes"));
        self.memory.crash_keeping(kept);
        self.totals.crashes += 1;
    }

    /// Open a new session, sometimes by another owner or a restarted one,
    /// sometimes with a fault while it reads, and find which of `candidates`
    /// it holds.
    async fn reopen(&mut self, candidates: &[Model]) -> std::result::Result<Session, String> {
        let handover = self.rng.percent(25);
        if handover {
            text(self.settle().await)?;
            self.totals.handovers += 1;
            self.owner = self.rng.pick(&[None, Some(0), Some(1)]);
            if let Some(owner) = self.owner
                && self.rng.percent(30)
            {
                self.caches[owner] = new_owner(&mut self.rng);
            }
        }
        let mut faulty = self.rng.percent(20);
        loop {
            if faulty {
                let _signal = self.store.arm(Failpoint {
                    operation: Operation::Get,
                    key_prefix: String::new(),
                    nth: 1 + self.rng.below(8) as usize,
                    effect: Effect::None,
                    cancel: false,
                });
            }
            let options = self.options();
            trace(|| {
                let o = &options.options;
                format!(
                    "open by {:?}{}: chunks of {}, rebase past {}, inline {}, prefetch {}",
                    self.owner,
                    if faulty { ", faulty" } else { "" },
                    o.chunk_pages,
                    o.rebase_ratio,
                    o.inline_record_bytes,
                    o.prefetch_pages,
                )
            });
            let found = async {
                let session = Session::open(self.store.clone(), options).await?;
                for (n, candidate) in candidates.iter().enumerate() {
                    if candidate.matches(&session).await? {
                        return Ok((session, n));
                    }
                }
                let differs = self.model.differs(&session).await?;
                Err(format!(
                    "none of {} states; from the last: {differs:?}",
                    candidates.len()
                )
                .into())
            };
            let found: Result<_> = found.await;
            self.store.disarm();
            match found {
                Ok((session, n)) => {
                    trace(|| format!("reopened on state {n} of {}", candidates.len()));
                    // What an owner reads is durable: it syncs at open unless
                    // its cache knows every flush it made settled.
                    if !self.memory.unsynced().is_empty() {
                        return Err("a reopen read writes no sync made durable".into());
                    }
                    self.observed(candidates[n].clone());
                    return Ok(session);
                }
                // The fault it was given: again, without.
                Err(error) if faulty && error.to_string().contains("injected") => faulty = false,
                Err(error) => return Err(format!("reopen: {error}")),
            }
        }
    }

    /// A reopen read `state`, or a flush made it.
    fn observed(&mut self, state: Model) {
        if self.memory.unsynced().is_empty() {
            self.since_sync.clear();
        }
        self.since_sync.push(state.clone());
        self.model = state;
    }

    /// One flush of a mutation of random changes, perhaps with a fault, a
    /// cancellation, a crash or a reopen. Returns the session that goes on.
    async fn step(&mut self, mut session: Session) -> std::result::Result<Session, String> {
        // The backend may crash at any time, and with it the owner's session:
        // what a reopen read but no sync made durable may go.
        if !self.memory.unsynced().is_empty() && self.rng.percent(30) {
            drop(session);
            self.crash();
            let candidates = self.since_sync.clone();
            session = self.reopen(&candidates).await?;
        }
        let mut mutation = session.state.mutation();
        let mut pending = self.model.clone();
        for _ in 0..1 + self.rng.below(8) {
            let op = match self.rng.percent(10) {
                true => self.model.truncation(&mut self.rng),
                false => Op::random(&mut self.rng),
            };
            trace(|| format!("  {}", describe(&op)));
            text(session.apply(&mut mutation, &op))?;
            pending.apply(&op);
            if self.rng.percent(30) {
                // The state reads as last flushed, whatever the mutation holds.
                let name = self.rng.pick(&FILES);
                let index = self.rng.below(self.model.len(name).div_ceil(4096) + 2);
                let read = text(session.file(name).read(index).await)?;
                if read != self.model.page(name, index) {
                    return Err(format!("{name}: page {index} not as flushed"));
                }
            }
        }
        if self.rng.percent(5) {
            trace(|| "discard".into());
            mutation = session.state.mutation();
            pending = self.model.clone();
        }
        let outcome = match self.rng.below(10) {
            0..=5 => mutation.flush().await,
            6..=7 => {
                let failpoint = self.failpoint();
                let _signal = self.store.arm(failpoint);
                let outcome = mutation.flush().await;
                self.store.disarm();
                outcome
            }
            _ => match self.cancelled(mutation) {
                Some(outcome) => outcome,
                // Never polled: nothing happened, and the mutation went with the
                // future.
                None => {
                    if !session.state.is_usable() {
                        return Err("a flush never polled ended the state".into());
                    }
                    return Ok(session);
                }
            },
        };
        self.totals.flushes += 1;
        trace(|| {
            let outcome = if outcome.is_ok() {
                "acknowledged"
            } else {
                "unacknowledged"
            };
            format!("flush: {outcome}")
        });
        if let Ok(report) = &outcome {
            self.totals.rebased += report.rebased;
            self.totals.sweeps += usize::from(report.swept.is_some());
        }
        if outcome.is_ok() {
            // One that wrote syncs; an unchanged one writes nothing, and
            // needs no sync: its session's open synced what it read.
            if !self.memory.unsynced().is_empty() {
                return Err("an acknowledged flush left writes unsynced".into());
            }
            self.observed(pending);
            text(self.read_beside(session.state.version()).await)?;
            if self.rng.percent(25) {
                session = self.sweep(session).await?;
            }
            // The session goes on, or a new one (after a crash, perhaps)
            // reads exactly what was acknowledged.
            if self.rng.percent(60) {
                if self.rng.percent(20) {
                    self.check(&session).await?;
                }
                return Ok(session);
            }
            drop(session);
            if self.rng.percent(30) {
                self.crash();
            }
            let candidates = self.since_sync.clone();
            return self.reopen(&candidates).await;
        }
        // The outcome is unknown: the state ended.
        if session.state.is_usable() || session.state.mutation().flush().await.is_ok() {
            return Err("a failed flush left the state usable".into());
        }
        drop(session);
        if self.rng.percent(50) {
            self.crash();
        }
        let before = self.model.clone();
        let mut candidates = self.since_sync.clone();
        candidates.push(pending.clone());
        let session = self.reopen(&candidates).await?;
        match (self.model == pending, self.model == before) {
            (true, false) => self.totals.landed += 1,
            (false, _) => self.totals.lost += 1,
            _ => {}
        }
        Ok(session)
    }

    /// After a flush by the owner, at `version`: each reader beside it reads
    /// what it opened, until the owner's flushes may have deleted its blobs
    /// (`garbage_after` versions on). Sometimes a new reader opens, on what
    /// is durable: a reader sees only what the store holds.
    async fn read_beside(&mut self, version: u64) -> Result<()> {
        let grace = self.garbage_after;
        let mut kept = Vec::new();
        for (reader, model) in std::mem::take(&mut self.readers) {
            // A flush at the next version, or one at this version that did
            // not land, deletes what died `grace` versions before it.
            let opened = reader.state.version();
            let may_be_gone = version >= opened + grace;
            match model.differs(&reader).await {
                Ok(None) => self.totals.reads_beside += 1,
                Ok(Some(differs)) => {
                    return Err(format!("a reader beside the owner: {differs}").into());
                }
                Err(error) if may_be_gone && error.to_string().contains("is gone") => continue,
                Err(error) => return Err(format!("a reader beside the owner: {error}").into()),
            }
            if version < opened + 2 * grace + 4 {
                kept.push((reader, model));
            }
        }
        self.readers = kept;
        if self.readers.len() < 3 && self.rng.percent(30) && self.memory.unsynced().is_empty() {
            let cache = self.rng.pick(&[None, Some(0), Some(1)]);
            let setup = Setup {
                options: StateOptions::default(),
                cache: cache.map(|owner| self.caches[owner].clone()),
            };
            let reader = Session::reader(self.store.clone(), setup).await?;
            self.readers.push((reader, self.model.clone()));
        }
        Ok(())
    }

    /// A sweep after an acknowledged flush, by the owner's state or by a
    /// worker beside it, sometimes with a fault. A failed sweep ends the
    /// state that ran it: the owner's reopens. Returns the session that goes
    /// on.
    async fn sweep(&mut self, session: Session) -> std::result::Result<Session, String> {
        let worker = match self.rng.percent(50) {
            true => None,
            false => {
                let setup = Setup {
                    options: StateOptions {
                        garbage_after: self.garbage_after,
                        ..StateOptions::default()
                    },
                    cache: None,
                };
                Some(text(Session::reader(self.store.clone(), setup).await)?)
            }
        };
        let faulty = self.rng.percent(30);
        if faulty {
            let rng = &mut self.rng;
            let (operation, key_prefix) = rng.pick(&[
                (Operation::Get, "v17/root"),
                (Operation::Delete, "v17/blob/"),
                (Operation::Put, "v17/sweep"),
                (Operation::Delete, "v17/sweep"),
                (Operation::Sync, ""),
            ]);
            let _signal = self.store.arm(Failpoint {
                operation,
                key_prefix: key_prefix.into(),
                nth: 1 + rng.below(3) as usize,
                effect: rng.pick(&[Effect::None, Effect::After]),
                cancel: false,
            });
        }
        let state = worker
            .as_ref()
            .map_or(&session.state, |worker| &worker.state);
        let swept = state.collect_garbage().await;
        self.store.disarm();
        self.totals.sweeps += 1;
        trace(|| {
            let by = if worker.is_some() {
                "a worker"
            } else {
                "the owner"
            };
            format!("sweep by {by}: {swept:?}")
        });
        let error = match swept {
            Ok(_) => return Ok(session),
            Err(error) if faulty && error.to_string().contains("injected") => error,
            Err(error) => return Err(format!("sweep: {error}")),
        };
        if state.is_usable() {
            return Err(format!("a failed sweep left the state usable: {error}"));
        }
        if worker.is_some() {
            // The worker's requests settle, in its own process.
            text(self.settle().await)?;
            return Ok(session);
        }
        drop(session);
        if self.rng.percent(50) {
            self.crash();
        }
        let candidates = self.since_sync.clone();
        self.reopen(&candidates).await
    }

    /// Sweep everything dead, as a fresh owner that keeps garbage a single
    /// version: what was dead at one sweep, the next one, a version on,
    /// deletes.
    async fn collect_all(&mut self) -> std::result::Result<(), String> {
        let setup = Setup {
            options: StateOptions {
                garbage_after: 1,
                sweep_every: 0,
                ..StateOptions::default()
            },
            cache: None,
        };
        let session = text(Session::open(self.store.clone(), setup).await)?;
        let mut swept = Vec::new();
        for tick in 0..3 {
            swept.push(text(session.state.collect_garbage().await)?);
            let ops = [Op::Set("progress", format!("tick {tick}").into())];
            text(session.flush(&ops).await)?;
        }
        if swept[2].waiting || swept[2].deleted != swept[1].dead {
            return Err(format!("a sweep left garbage: {swept:#?}"));
        }
        Ok(())
    }

    /// A random failpoint; it may match nothing the flush does.
    fn failpoint(&mut self) -> Failpoint {
        let rng = &mut self.rng;
        let operation = rng.pick(&[
            Operation::Put,
            Operation::Put,
            Operation::Delete,
            Operation::Sync,
        ]);
        let key_prefix = match operation {
            Operation::Sync => "",
            _ => rng.pick(&["", "", "v17/root", "v17/blob/"]),
        };
        Failpoint {
            operation,
            key_prefix: key_prefix.into(),
            nth: 1 + rng.below(4) as usize,
            effect: rng.pick(&[Effect::None, Effect::After]),
            cancel: false,
        }
    }

    /// Poll the flush of `mutation` a random number of times, then drop it:
    /// `None` if it was never polled.
    fn cancelled(&mut self, mutation: Mutation) -> Option<Result<FlushReport>> {
        let polls = self.rng.below(12);
        let mut flush = Box::pin(mutation.flush());
        let mut cx = Context::from_waker(noop_waker_ref());
        for _ in 0..polls {
            if let Poll::Ready(outcome) = flush.as_mut().poll(&mut cx) {
                return Some(outcome);
            }
        }
        self.totals.cancelled += usize::from(polls > 0);
        (polls > 0).then(|| Err("cancelled".into()))
    }

    /// The session reads as the model.
    async fn check(&self, session: &Session) -> std::result::Result<(), String> {
        match self.model.differs(session).await {
            Ok(None) => Ok(()),
            Ok(Some(differs)) => Err(format!("the session differs: {differs}")),
            Err(error) => Err(format!("the session fails to read: {error}")),
        }
    }
}

/// With `STATE_TRACE` set, print what a run does.
fn trace(line: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("STATE_TRACE").is_some()) {
        eprintln!("{}", line());
    }
}

fn describe(op: &Op) -> String {
    match op {
        Op::Set(name, value) => format!("set {name} to {} bytes", value.len()),
        Op::Write(name, index, Some(image)) => {
            format!("write {name} page {index}: {:02x?}", &image[..4])
        }
        Op::Write(name, index, None) => format!("hole {name} page {index}"),
        Op::SetLen(name, len) => format!("set_len {name} {len}"),
        Op::Append(name, entry) => format!("append {name} {} bytes", entry.len()),
        Op::Truncate(name, before) => format!("truncate {name} before {before}"),
    }
}

/// A result whose error is text, for a failing seed to print.
fn text<T>(result: Result<T>) -> std::result::Result<T, String> {
    result.map_err(|error| error.to_string())
}

async fn run(seed: u64) -> std::result::Result<Totals, String> {
    let mut rng = Rng::new(seed);
    // Odd seeds leave blobs unfenced, as in S3 beside Restate: the owner
    // claims its numbers, and no reopen after a crash or a fault may put a
    // blob number twice.
    let memory = match seed % 2 {
        1 => MemoryKv::unfenced(),
        _ => MemoryKv::default(),
    };
    let fault = FaultKv::new(memory.clone(), rng.below(3));
    let store = Arc::new(MonitorKv::new(fault).with_base("vm", golden()));
    let caches = [new_owner(&mut rng), new_owner(&mut rng)];
    let mut world = World {
        rng,
        store,
        memory,
        caches,
        owner: Some(0),
        model: Model::default(),
        since_sync: vec![Model::default()],
        garbage_after: 0,
        readers: Vec::new(),
        totals: Totals::default(),
    };
    world.garbage_after = world.rng.pick(&[1, 2, 4, 16]);
    let mut session = world.reopen(&[Model::default()]).await?;
    for step in 0..STEPS {
        session = (world.step(session).await).map_err(|e| format!("step {step}: {e}"))?;
        world
            .violations()
            .map_err(|e| format!("step {step}: {e}"))?;
    }
    // Whatever happened, a crash and a cold open read the last state.
    drop(session);
    text(world.settle().await)?;
    world.owner = None;
    let candidates = world.since_sync.clone();
    let session = world.reopen(&candidates).await?;
    world.check(&session).await?;
    drop(session);
    world.collect_all().await?;
    world.violations()?;
    let stats = world.memory.stats();
    world.totals.requests_checked = world.store.checked();
    world.totals.blobs = stats.kind("blob").puts;
    world.totals.collected = stats.kind("blob").deletes;
    Ok(world.totals)
}

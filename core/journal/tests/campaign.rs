//! A seeded random campaign over the public API: changes to several files
//! (one with a base) and records, commits with tiny thresholds so that seals,
//! checkpoints, garbage deletion and the working-set hint all happen, and
//! random faults, cancellations, crashes and reopens by owners with and
//! without a cache, every request checked by the protocol monitor. After
//! each reopen the journal reads as the model: every acknowledged commit,
//! and an unacknowledged one wholly or not at all.
//!
//! ```sh
//! cargo test --features simulation --test campaign                  # a few seeds
//! JOURNAL_SEEDS=1000 cargo test --release --features simulation --test campaign
//! JOURNAL_SEED=17 cargo test --features simulation --test campaign  # one, as a failure prints it
//! ```
#![cfg(feature = "simulation")]
mod support;

use futures::task::noop_waker_ref;
use journal::{
    NamespaceCache, PageCache, PageCacheConfig, PageCompression, Result, StorageOptions,
    kv::KvStore, simulation::*,
};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    task::{Context, Poll},
};
use support::*;

/// Seeds `cargo test` runs without `JOURNAL_SEEDS`.
const SEEDS: u64 = 12;
/// Commits per seed.
const STEPS: usize = 40;

type Store = MonitorKv<FaultKv>;

/// The seeds to run: `JOURNAL_SEED`, or the first `JOURNAL_SEEDS`.
fn seeds() -> Vec<u64> {
    let var = |name| {
        std::env::var(name)
            .ok()
            .map(|v: String| v.parse().expect(name))
    };
    match (var("JOURNAL_SEED"), var("JOURNAL_SEEDS")) {
        (Some(seed), _) => vec![seed],
        (None, count) => (0..count.unwrap_or(SEEDS)).collect(),
    }
}

#[test]
fn random_faults_never_lose_an_acknowledged_commit_or_split_one() {
    let mut totals = Totals::default();
    for seed in seeds() {
        let run = std::panic::catch_unwind(|| futures::executor::block_on(run(seed)));
        let rerun = format!(
            "JOURNAL_SEED={seed} cargo test --features simulation --test campaign -- --nocapture"
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
            t.sealed > 0 && t.collected > 0 && t.checkpointed > 0 && t.hints > 0,
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
    commits: usize,
    /// Unacknowledged commits found whole after a reopen, and found absent.
    landed: usize,
    lost: usize,
    crashes: usize,
    cancelled: usize,
    handovers: usize,
    requests_checked: usize,
    /// Stored values put (seals, large transactions and records), deleted
    /// as garbage, checkpointed page writes, and working-set hints.
    sealed: u64,
    collected: u64,
    checkpointed: u64,
    hints: u64,
}

impl Totals {
    fn add(&mut self, run: &Totals) {
        self.commits += run.commits;
        self.landed += run.landed;
        self.lost += run.lost;
        self.crashes += run.crashes;
        self.cancelled += run.cancelled;
        self.handovers += run.handovers;
        self.requests_checked += run.requests_checked;
        self.sealed += run.sealed;
        self.collected += run.collected;
        self.checkpointed += run.checkpointed;
        self.hints += run.hints;
    }
}

/// A cache that keeps decoded pages, for some owners.
fn decoded_cache() -> &'static PageCache {
    static CACHE: OnceLock<PageCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        open_cache(PageCacheConfig {
            decoded: true,
            ..cache_config(4 << 20, 16 << 20)
        })
    })
}

/// A new owner process's cache: its first look at the namespace.
fn new_owner(rng: &mut Rng) -> NamespaceCache {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    match rng.percent(30) {
        true => decoded_cache().namespace(&format!("campaign-{}", KEYS.fetch_add(1, Relaxed))),
        false => fresh_namespace(),
    }
}

/// One seed's world: the store, the owners, and what the journal may hold.
struct World {
    rng: Rng,
    store: Arc<Store>,
    memory: MemoryKv,
    /// Two owner processes, each with its own cache.
    caches: [NamespaceCache; 2],
    /// The owner of the namespace: a process, or one without a cache.
    owner: Option<usize>,
    /// The state as of the last acknowledged commit or reopen.
    model: Model,
    /// The states a crash may go back to: the durable one, then each one
    /// read or committed since the last sync.
    since_sync: Vec<Model>,
    totals: Totals,
}

impl World {
    /// Random options: they never change the stored format.
    fn options(&mut self) -> StorageOptions {
        let cache = self.owner.map(|owner| self.caches[owner].clone());
        let rng = &mut self.rng;
        StorageOptions {
            page_compression: rng.pick(&[
                PageCompression::Raw,
                PageCompression::Lz4,
                PageCompression::Zstd,
            ]),
            cache,
            checkpoint_after: rng.below(4) as u32,
            checkpoint_log_bytes: rng.pick(&[4 << 10, 32 << 10, journal::CHECKPOINT_LOG_BYTES]),
            inline_log_bytes: rng.pick(&[0, 200, 1024, 8192]),
            prefetch_pages: rng.pick(&[0, 4, 16]),
            hint_pages: rng.pick(&[0, 8, 32]),
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
                let o = &options;
                format!(
                    "open by {:?}{}: checkpoint after {} or {} bytes, inline {}, prefetch {}, hint {}",
                    self.owner,
                    if faulty { ", faulty" } else { "" },
                    o.checkpoint_after,
                    o.checkpoint_log_bytes,
                    o.inline_log_bytes,
                    o.prefetch_pages,
                    o.hint_pages
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
                    self.observed(candidates[n].clone());
                    return Ok(session);
                }
                // The fault it was given: again, without.
                Err(error) if faulty && error.to_string().contains("injected") => faulty = false,
                Err(error) => return Err(format!("reopen: {error}")),
            }
        }
    }

    /// A reopen read `state`, or a commit made it.
    fn observed(&mut self, state: Model) {
        if self.memory.unsynced().is_empty() {
            self.since_sync.clear();
        }
        self.since_sync.push(state.clone());
        self.model = state;
    }

    /// One commit of a batch of random changes, perhaps with a fault, a
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
        let mut pending = self.model.clone();
        for _ in 0..1 + self.rng.below(8) {
            let op = Op::random(&mut self.rng);
            trace(|| format!("  {}", describe(&op)));
            text(session.apply(&op))?;
            pending.apply(&op);
            if self.rng.percent(30) {
                let name = self.rng.pick(&FILES);
                let index = self.rng.below(pending.len(name).div_ceil(4096) + 2);
                let read = text(session.file(name).read(index).await)?;
                if read != pending.page(name, index) {
                    return Err(format!("{name}: page {index} not as written"));
                }
            }
        }
        if self.rng.percent(5) {
            trace(|| "discard".into());
            session.journal.discard();
            pending = self.model.clone();
        }
        let outcome = match self.rng.below(10) {
            0..=5 => Some(session.journal.commit().await),
            6..=7 => {
                let failpoint = self.failpoint();
                let _signal = self.store.arm(failpoint);
                let outcome = session.journal.commit().await;
                self.store.disarm();
                Some(outcome)
            }
            _ => self.cancelled(&session),
        };
        let acknowledged = match outcome {
            // Never polled: nothing happened, and the changes wait.
            None => {
                if !session.journal.is_usable() {
                    return Err("a commit never polled ended the journal".into());
                }
                Some(text(session.journal.commit().await)?)
            }
            Some(Ok(report)) => Some(report),
            Some(Err(_)) => None,
        };
        self.totals.commits += 1;
        trace(|| {
            format!(
                "commit: {:?}",
                acknowledged.as_ref().map(|_| "acknowledged")
            )
        });
        if let Some(report) = acknowledged {
            // One that wrote syncs; an unchanged one writes nothing, and
            // syncs nothing either.
            let synced = self.memory.unsynced().is_empty();
            if report.record_bytes > 0 && !synced {
                return Err("an acknowledged commit left writes unsynced".into());
            }
            self.observed(pending);
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
        // The outcome is unknown: the journal ended.
        if session.journal.is_usable() || session.journal.commit().await.is_ok() {
            return Err("a failed commit left the journal usable".into());
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

    /// A random failpoint; it may match nothing the commit does.
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
            _ => rng.pick(&["", "", "v15/commit", "v15/log/", "v15/page/", "v15/hint"]),
        };
        Failpoint {
            operation,
            key_prefix: key_prefix.into(),
            nth: 1 + rng.below(4) as usize,
            effect: rng.pick(&[Effect::None, Effect::After]),
            cancel: false,
        }
    }

    /// Poll a commit a random number of times, then drop it: `None` if it
    /// was never polled.
    fn cancelled(&mut self, session: &Session) -> Option<Result<journal::CommitReport>> {
        let polls = self.rng.below(12);
        let mut commit = Box::pin(session.journal.commit());
        let mut cx = Context::from_waker(noop_waker_ref());
        for _ in 0..polls {
            if let Poll::Ready(outcome) = commit.as_mut().poll(&mut cx) {
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

/// With `JOURNAL_TRACE` set, print what a run does.
fn trace(line: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("JOURNAL_TRACE").is_some()) {
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
    }
}

/// A result whose error is text, for a failing seed to print.
fn text<T>(result: Result<T>) -> std::result::Result<T, String> {
    result.map_err(|error| error.to_string())
}

async fn run(seed: u64) -> std::result::Result<Totals, String> {
    let mut rng = Rng::new(seed);
    let memory = MemoryKv::default();
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
        totals: Totals::default(),
    };
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
    world.violations()?;
    let stats = world.memory.stats();
    world.totals.requests_checked = world.store.checked();
    world.totals.sealed = stats.kind("log").puts;
    world.totals.collected = stats.kind("log").deletes;
    world.totals.checkpointed = stats.kind("page").puts;
    world.totals.hints = stats.kind("hint").puts;
    Ok(world.totals)
}

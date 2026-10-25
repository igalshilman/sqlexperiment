//! The root in Restate, the blobs in a blob store that fails as S3 does
//! ([`SplitStore`] over [`MemoryBlobs::faults`]): of a flush's uploads,
//! some land and some fail, some land after their attempt failed; Restate
//! fails syncs, before or after storing; new attempts fence old ones, which
//! go on as zombies, flushing and sweeping; owners and workers sweep, with
//! faults of their own.
//!
//! Each seed checks that no blob key is ever put twice, that every open
//! reads the acknowledged state (or the one a failed flush made), and, once
//! everything in flight landed, that a few sweeps leave only what the root
//! names.
//!
//! ```sh
//! cargo test --features simulation --test split_campaign
//! STATE_SEEDS=1000 cargo test --release --features simulation --test split_campaign
//! STATE_SEED=7 STATE_TRACE=1 cargo test --features simulation --test split_campaign -- --nocapture
//! ```
#![cfg(feature = "simulation")]
mod support;

use state::{Result, StateOptions, blobs::SplitStore, kv::KvStore, simulation::*};
use std::sync::{Arc, OnceLock};
use support::*;

const SEEDS: u64 = 64;
const STEPS: usize = 80;

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

/// With `STATE_TRACE` set, print what a run does.
fn trace(line: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("STATE_TRACE").is_some()) {
        eprintln!("{}", line());
    }
}

#[test]
fn partial_uploads_retries_and_zombies_never_lose_a_flush_or_put_a_blob_twice() {
    let mut totals = Totals::default();
    for seed in seeds() {
        let run = futures::executor::block_on(run(seed));
        match run {
            Ok(run) => totals.add(&run),
            Err(error) => panic!(
                "seed {seed}: {error}\nrerun: STATE_SEED={seed} STATE_TRACE=1 cargo test \
                 --features simulation --test split_campaign -- --nocapture"
            ),
        }
    }
    println!("{totals:?}");
    if seeds().len() as u64 >= SEEDS {
        let t = &totals;
        assert!(t.flushed > 0 && t.failed > 0 && t.landed > 0, "{t:?}");
        assert!(
            t.zombie_blobs > 0 && t.late > 0 && t.blob_faults > 0,
            "{t:?}"
        );
        assert!(t.swept > 0 && t.sweeps_failed > 0, "{t:?}");
    }
}

/// What the seeds did, to check that they cover what they should.
#[derive(Debug, Default)]
struct Totals {
    /// Flushes acknowledged, failed, and failed but found landed.
    flushed: usize,
    failed: usize,
    landed: usize,
    /// Flushes the owner's cache refused, after a zombie's flush.
    refused: usize,
    /// Blobs zombies put, after a later attempt fenced them.
    zombie_blobs: u64,
    /// Blob writes that landed after their caller gave up on them.
    late: u64,
    /// Blob requests the faults failed.
    blob_faults: u64,
    /// Blobs sweeps deleted, and sweeps that failed.
    swept: usize,
    sweeps_failed: usize,
}

impl Totals {
    fn add(&mut self, run: &Totals) {
        self.flushed += run.flushed;
        self.failed += run.failed;
        self.landed += run.landed;
        self.refused += run.refused;
        self.zombie_blobs += run.zombie_blobs;
        self.late += run.late;
        self.blob_faults += run.blob_faults;
        self.swept += run.swept;
        self.sweeps_failed += run.sweeps_failed;
    }
}

/// One seed's world.
struct World {
    rng: Rng,
    restate: Restate,
    blobs: MemoryBlobs,
    options: StateOptions,
    /// The owner process's cache, across its attempts, if it has one.
    cache: Option<Namespace>,
    /// The acknowledged state, and the one a flush that failed may have
    /// made: the next open finds which.
    model: Model,
    pending: Option<Model>,
    /// The current attempt's session, if it has one open.
    owner: Option<Session>,
    /// Earlier attempts' sessions, fenced and still running.
    zombies: Vec<Session>,
    totals: Totals,
}

impl World {
    /// A store over a new attempt, which fences every earlier one.
    fn attempt(&self) -> Arc<dyn KvStore> {
        let blobs = Arc::new(self.blobs.clone());
        Arc::new(SplitStore::new(self.restate.attempt(), blobs))
    }

    /// A store that reads beside the attempts: a worker's.
    fn worker(&self) -> Arc<dyn KvStore> {
        let blobs = Arc::new(self.blobs.clone());
        Arc::new(SplitStore::new(self.restate.reader(), blobs))
    }

    /// Fail blob requests at random, one in `one_in`.
    fn faults(&mut self) {
        let one_in = self.rng.pick(&[2, 3, 6, 12]);
        let seed = self.rng.next();
        self.blobs.faults(Some(BlobFaults { one_in, seed }));
    }

    fn heal(&mut self) {
        self.blobs.faults(None);
        self.restate.fail_next_sync(None);
    }

    fn setup(&self) -> Setup {
        Setup {
            options: self.options.clone(),
            cache: self.cache.clone(),
        }
    }

    /// The state a cold reader finds: the model, or the one a failed flush
    /// made. Settles which.
    async fn settle(&mut self) -> std::result::Result<(), String> {
        let reader = text(Session::reader(self.worker(), StateOptions::default()).await)?;
        if let Some(pending) = self.pending.take()
            && text(pending.matches(&reader).await)?
        {
            trace(|| "the failed flush landed".into());
            self.totals.landed += 1;
            self.model = pending;
            return Ok(());
        }
        match text(self.model.differs(&reader).await)? {
            None => Ok(()),
            Some(differs) => Err(format!("a cold reader: {differs}")),
        }
    }

    /// A new attempt opens the state, the earlier one going on as a zombie.
    /// Its open may fail, at a blob read: then another attempt opens it.
    async fn open(&mut self) -> std::result::Result<(), String> {
        if let Some(owner) = self.owner.take()
            && owner.state.is_usable()
        {
            self.zombies.push(owner);
        }
        self.zombies.truncate(3);
        for tries in 0.. {
            let faulty = tries < 2 && self.rng.percent(30);
            if faulty {
                self.faults();
            }
            let opened = Session::open(self.attempt(), self.setup()).await;
            self.heal();
            match opened {
                Ok(session) => {
                    trace(|| format!("attempt opens{}", if faulty { ", faulty" } else { "" }));
                    self.settle().await?;
                    if let Some(differs) = text(self.model.differs(&session).await)? {
                        return Err(format!("the owner, opened: {differs}"));
                    }
                    self.owner = Some(session);
                    return Ok(());
                }
                Err(error) if faulty => trace(|| format!("attempt fails to open: {error}")),
                Err(error) => return Err(format!("open: {error}")),
            }
        }
        unreachable!()
    }

    fn ops(&mut self) -> Vec<Op> {
        (0..1 + self.rng.below(6))
            .map(|_| match self.rng.percent(10) {
                true => self.model.truncation(&mut self.rng),
                false => Op::random(&mut self.rng),
            })
            .collect()
    }

    /// The owner flushes, perhaps with blob faults and a failed Restate
    /// sync.
    async fn flush(&mut self) -> std::result::Result<(), String> {
        if self.owner.is_none() {
            self.open().await?;
        }
        let ops = self.ops();
        let faulty = self.rng.percent(40);
        if faulty {
            match self.rng.below(3) {
                0 => self.faults(),
                1 => {
                    let effect = self.rng.pick(&[Effect::None, Effect::After]);
                    self.restate.fail_next_sync(Some(effect));
                }
                _ => {
                    self.faults();
                    self.restate.fail_next_sync(Some(Effect::After));
                }
            }
        }
        let owner = self.owner.as_ref().unwrap();
        let flushed = owner.flush(&ops).await;
        let usable = owner.state.is_usable();
        self.heal();
        let pending = self.model.applied(&ops);
        match flushed {
            Ok(report) => {
                trace(|| format!("flush: acknowledged, swept: {:?}", report.swept));
                self.totals.flushed += 1;
                self.totals.swept += report.swept.map_or(0, |swept| swept.deleted);
                self.model = pending;
                Ok(())
            }
            // A zombie in this process flushed through the owner's cache:
            // nothing written, the attempt fails, and Restate retries.
            Err(error) if error.to_string().contains("another state flushed") => {
                trace(|| format!("flush: refused: {error}"));
                self.totals.refused += 1;
                self.owner = None;
                Ok(())
            }
            Err(error) if faulty && !usable => {
                trace(|| format!("flush: failed: {error}"));
                self.totals.failed += 1;
                self.pending = Some(pending);
                // The attempt fails with it; Restate retries.
                self.owner = None;
                Ok(())
            }
            Err(error) if faulty => Err(format!("a failed flush left the state usable: {error}")),
            Err(error) => Err(format!("flush: {error}")),
        }
    }

    /// A zombie flushes: its blobs may land, within its claim, but its root
    /// never does.
    async fn zombie_flush(&mut self) -> std::result::Result<(), String> {
        let Some(n) = (!self.zombies.is_empty()).then(|| self.rng.below(self.zombies.len() as u64))
        else {
            return Ok(());
        };
        let ops = self.ops();
        let before = self.blobs.puts();
        let zombie = &self.zombies[n as usize];
        let flushed = zombie.flush(&ops).await;
        let put = self.blobs.puts() - before;
        self.totals.zombie_blobs += put;
        trace(|| format!("zombie flush: {flushed:?}, {put} blobs put"));
        match flushed {
            Ok(report) if report.root_bytes > 0 => Err("a zombie's flush was acknowledged".into()),
            _ => Ok(()),
        }
    }

    /// A sweep: by the owner, a worker beside it, or a zombie, perhaps with
    /// faults. A zombie's fails: it cannot make Restate's root durable.
    async fn sweep(&mut self) -> std::result::Result<(), String> {
        let by = self.rng.pick(&["owner", "worker", "worker", "zombie"]);
        let worker;
        let state = match by {
            "owner" => match &self.owner {
                Some(owner) => &owner.state,
                None => return Ok(()),
            },
            "zombie" => match self.zombies.first() {
                Some(zombie) => &zombie.state,
                None => return Ok(()),
            },
            _ => {
                let options = self.options.clone();
                worker = text(Session::reader(self.worker(), options).await)?;
                &worker.state
            }
        };
        let faulty = self.rng.percent(40);
        if faulty {
            let one_in = self.rng.pick(&[2, 3, 6, 12]);
            let seed = self.rng.next();
            self.blobs.faults(Some(BlobFaults { one_in, seed }));
        }
        let swept = state.collect_garbage().await;
        let usable = state.is_usable();
        self.heal();
        trace(|| {
            format!(
                "sweep by a {by}{}: {swept:?}",
                if faulty { ", faulty" } else { "" }
            )
        });
        match swept {
            Ok(swept) if by == "zombie" => Err(format!("a zombie swept: {swept:?}")),
            Ok(swept) => {
                self.totals.swept += swept.deleted;
                Ok(())
            }
            Err(error) if usable => Err(format!("a failed sweep left the state usable: {error}")),
            Err(_) if by == "zombie" || faulty => {
                self.totals.sweeps_failed += 1;
                if by == "owner" {
                    self.owner = None;
                }
                Ok(())
            }
            Err(error) => Err(format!("sweep: {error}")),
        }
    }

    /// What is in flight lands.
    fn land(&mut self) {
        let late = self.blobs.in_flight();
        trace(|| format!("{late} late blob writes land"));
        self.blobs.land_late();
    }

    /// Everything in flight lands; a new attempt opens, and sweeps until
    /// only what its root names is stored: nothing leaked for good.
    async fn finish(&mut self) -> std::result::Result<(), String> {
        self.land();
        self.options = StateOptions {
            garbage_after: 1,
            sweep_every: 0,
            ..self.options.clone()
        };
        self.open().await?;
        let owner = self.owner.as_ref().unwrap();
        let mut swept = Vec::new();
        for tick in 0..3 {
            swept.push(text(owner.state.collect_garbage().await)?);
            let ops = [Op::Set("progress", format!("tick {tick}").into())];
            text(owner.flush(&ops).await)?;
            self.model = self.model.applied(&ops);
        }
        let last = &swept[2];
        if last.waiting
            || last.deleted != swept[1].dead
            || last.listed - last.deleted - last.dead != last.named
        {
            return Err(format!("sweeps left blobs no root will name: {swept:#?}"));
        }
        self.pending = None;
        self.settle().await
    }

    fn check(&self) -> std::result::Result<(), String> {
        match self.blobs.overwrites() {
            0 => Ok(()),
            n => Err(format!("{n} blob keys put twice")),
        }
    }
}

fn text<T>(result: Result<T>) -> std::result::Result<T, String> {
    result.map_err(|error| error.to_string())
}

async fn run(seed: u64) -> std::result::Result<Totals, String> {
    let mut rng = Rng::new(seed);
    let options = StateOptions {
        chunk_pages: rng.pick(&[1, 2, 4]),
        fanout: rng.pick(&[2, 3, 64]),
        rebase_ratio: rng.pick(&[0.0, 0.25, 1.0]),
        inline_record_bytes: rng.pick(&[0, 200, 1024]),
        prefetch_pages: rng.pick(&[0, 16]),
        garbage_after: rng.pick(&[1, 2, 4]),
        sweep_every: rng.pick(&[0, 1, 3]),
    };
    let cache = rng.percent(50).then(fresh_namespace);
    let mut world = World {
        rng,
        restate: Restate::default(),
        blobs: MemoryBlobs::default(),
        options,
        cache,
        model: Model::default(),
        pending: None,
        owner: None,
        zombies: Vec::new(),
        totals: Totals::default(),
    };
    world.open().await?;
    for step in 0..STEPS {
        let done = match world.rng.below(20) {
            0..=9 => world.flush().await,
            10..=11 => world.open().await,
            12..=13 => world.zombie_flush().await,
            14..=16 => world.sweep().await,
            17 => {
                world.land();
                Ok(())
            }
            _ => world.settle().await,
        };
        done.and_then(|()| world.check())
            .map_err(|error| format!("step {step}: {error}"))?;
    }
    world.finish().await?;
    world.check()?;
    world.totals.late = world.blobs.landed_late();
    world.totals.blob_faults = world.blobs.failed();
    Ok(world.totals)
}

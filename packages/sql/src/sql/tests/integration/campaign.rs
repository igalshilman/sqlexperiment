//! A seeded, deterministic, randomized campaign: random SQL (tables, indexes,
//! rows past a page, transactions that commit or roll back, statements that
//! fail partway through a string) through the bindings' session and through
//! `Sql` with a flush per turn, with random storage faults (an error
//! before or after a request, a cancellation, a crash keeping any prefix of
//! the writes) and reopens, over the protocol monitor, with and without the
//! page cache, and with random frame sizes and checkpoint intervals.
//!
//! After every reopen the store must hold what a reference engine holds that
//! replays only acknowledged transactions: every acknowledged commit, and an
//! unacknowledged one whole or not at all. Seeds run from 0; a failure names
//! its seed:
//!
//! ```sh
//! SQL_CAMPAIGN_SEEDS=500 cargo test --release sql::tests::integration::campaign
//! SQL_CAMPAIGN_SEED=17 SQL_CAMPAIGN_TRACE=1 cargo test sql::tests::integration::campaign -- --nocapture
//! ```
use super::host::StateSetup;
use super::{
    fault::{Ended, under},
    reference::{dump, dump_of, integrity, reference_dump, replay, run, text},
};
use crate::sql::{Rows, Sql, binding::Session};
use crate::state::{
    HEAD_KEY, Result, State, StateOptions, TAIL_PREFIX,
    blobs::KV_PREFIX,
    kv::KvStore,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, MonitorKv, Operation, fresh_namespace},
};
use bytes::Bytes;
use std::{panic, sync::Arc};
use turso_core::Connection;

/// Seeds to run by default: enough to cover each configuration.
const DEFAULT_SEEDS: u64 = 4;
/// Calls a seed makes.
const STEPS: usize = 120;

#[test]
fn random_workloads_with_faults_match_a_reference_engine() {
    let env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    let seeds = match env("SQL_CAMPAIGN_SEED") {
        Some(seed) => seed..seed + 1,
        None => 0..env("SQL_CAMPAIGN_SEEDS").unwrap_or(DEFAULT_SEEDS),
    };
    let trace = env("SQL_CAMPAIGN_TRACE").is_some();
    let mut totals = Totals::default();
    for seed in seeds {
        let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            futures::executor::block_on(Campaign::new(seed, trace).run())
        }));
        match outcome {
            Ok(Ok(run)) => totals.add(&run),
            Ok(Err(error)) => panic!(
                "campaign seed {seed}: {error}; rerun with SQL_CAMPAIGN_SEED={seed} SQL_CAMPAIGN_TRACE=1"
            ),
            Err(panicked) => {
                eprintln!(
                    "campaign seed {seed} failed; rerun with SQL_CAMPAIGN_SEED={seed} SQL_CAMPAIGN_TRACE=1"
                );
                panic::resume_unwind(panicked);
            }
        }
    }
    println!("campaign: {totals:?}");
}

/// What the seeds did.
#[derive(Debug, Default)]
struct Totals {
    seeds: u64,
    calls: usize,
    faults: usize,
    reopens: usize,
    crashes: usize,
    unknown_outcomes: usize,
}

impl Totals {
    fn add(&mut self, run: &Totals) {
        self.seeds += 1;
        self.calls += run.calls;
        self.faults += run.faults;
        self.reopens += run.reopens;
        self.crashes += run.crashes;
        self.unknown_outcomes += run.unknown_outcomes;
    }
}

/// A deterministic random source (xorshift).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// The reference: the SQL of acknowledged commits, and what ran since.
struct Model {
    acknowledged: Vec<String>,
    pending: Vec<String>,
    /// A reference engine that ran both.
    live: Arc<Connection>,
    /// The record `effects`, as acknowledged, and as set since.
    effects: Bytes,
    pending_effects: Option<Bytes>,
}

impl Model {
    fn new() -> Self {
        Self {
            acknowledged: Vec::new(),
            pending: Vec::new(),
            live: replay(&[]),
            effects: Bytes::new(),
            pending_effects: None,
        }
    }

    fn run(&mut self, sql: &str) -> std::result::Result<Rows, String> {
        self.pending.push(sql.into());
        run(&self.live, sql)
    }

    fn in_transaction(&self) -> bool {
        !self.live.get_auto_commit()
    }

    /// The pending SQL and effects were acknowledged.
    fn acknowledge(&mut self) {
        self.acknowledged.append(&mut self.pending);
        if let Some(effects) = self.pending_effects.take() {
            self.effects = effects;
        }
    }

    /// The state acknowledged, and with what is pending.
    fn acknowledged_state(&self) -> (String, Bytes) {
        (
            reference_dump(&replay(&self.acknowledged)),
            self.effects.clone(),
        )
    }
    fn pending_state(&self) -> (String, Bytes) {
        let all: Vec<String> = (self.acknowledged.iter().chain(&self.pending))
            .cloned()
            .collect();
        let effects = self.pending_effects.clone();
        (
            reference_dump(&replay(&all)),
            effects.unwrap_or(self.effects.clone()),
        )
    }

    /// The session ended: the store holds `seen`, which must be the state
    /// acknowledged or, when the outcome of a flush is `unknown`, the one
    /// with what is pending.
    fn settle(&mut self, seen: &(String, Bytes), unknown: bool, case: &str) {
        let old = self.acknowledged_state();
        if *seen == old {
            self.pending.clear();
            self.pending_effects = None;
        } else if unknown && *seen == self.pending_state() {
            self.acknowledge();
        } else {
            if std::env::var_os("SQL_CAMPAIGN_TRACE").is_some() {
                println!(
                    "acknowledged SQL: {:#?}\nstore:\n{}\nreference:\n{}",
                    self.acknowledged, seen.0, old.0
                );
            }
            let (seen_lines, old_lines): (Vec<_>, Vec<_>) =
                (seen.0.lines().collect(), old.0.lines().collect());
            let first = (0..seen_lines.len().max(old_lines.len()))
                .find(|&i| seen_lines.get(i) != old_lines.get(i))
                .unwrap_or(0);
            panic!(
                "{case}: the store holds neither the acknowledged state{} nor the pending one ({unknown}).\n\
                 First difference from the acknowledged at line {first}:\n  store:     {:?}\n  reference: {:?}\n\
                 effects: store {:?}, acknowledged {:?}, pending {:?}\npending SQL: {:?}",
                if unknown {
                    ""
                } else {
                    " (no write was issued)"
                },
                seen_lines.get(first),
                old_lines.get(first),
                seen.1,
                old.1,
                self.pending_effects,
                self.pending,
            );
        }
        self.live = replay(&self.acknowledged);
    }
}

/// How a seed runs.
struct Config {
    /// Through the bindings' session; otherwise `Sql`, a flush per turn.
    session: bool,
    cached: bool,
    options: StateSetup,
    /// The above, to print.
    summary: String,
}

/// The session the workload runs on.
enum Open {
    Session(Session),
    Turns { state: State, sql: Sql },
}

impl Open {
    fn is_usable(&self) -> bool {
        match self {
            Open::Session(session) => session.is_usable(),
            Open::Turns { sql, .. } => sql.is_usable(),
        }
    }

    async fn dump(&mut self) -> Result<(String, Bytes)> {
        match self {
            Open::Session(session) => {
                let state = dump_of(async |sql| Ok(session.query(sql, None).await?.1)).await?;
                Ok((state, session.effects().await?))
            }
            Open::Turns { state, sql } => Ok((dump(sql).await?, state.record("effects").get())),
        }
    }
}

struct Campaign {
    seed: u64,
    trace: bool,
    rng: Rng,
    config: Config,
    memory: MemoryKv,
    store: Arc<MonitorKv<FaultKv>>,
    model: Model,
    turn: u64,
    /// Between the session's `begin` and its `commit` or `rollback`.
    in_body: bool,
    totals: Totals,
}

impl Campaign {
    fn new(seed: u64, trace: bool) -> Self {
        let mut rng = Rng::new(seed);
        let cached = seed % 4 >= 2;
        let options = StateSetup {
            options: StateOptions {
                frame_pages: rng.pick(&[1, 4, 16]),
                checkpoint_entries: 1 + rng.below(4),
                checkpoint_bytes: rng.pick(&[0, 1 << 20]),
                inline_record_bytes: rng.pick(&[64, 1024]),
                prefetch_pages: rng.pick(&[0, 16]),
                ..StateOptions::default()
            },
            cache: cached.then(fresh_namespace),
            packs: None,
        };
        let session = seed.is_multiple_of(2);
        let summary = format!(
            "{}, cached: {cached}, frames of {} pages, a checkpoint every {} entries or {} bytes (0: by count only), inline records to {} bytes, prefetch {}",
            if session {
                "binding session"
            } else {
                "Sql turns"
            },
            options.options.frame_pages,
            options.options.checkpoint_entries,
            options.options.checkpoint_bytes,
            options.options.inline_record_bytes,
            options.options.prefetch_pages,
        );
        let config = Config {
            session,
            cached,
            options,
            summary,
        };
        let memory = MemoryKv::default();
        let store = Arc::new(MonitorKv::new(FaultKv::new(memory.clone(), seed)));
        Self {
            seed,
            trace,
            rng,
            config,
            memory,
            store,
            model: Model::new(),
            turn: 0,
            in_body: false,
            totals: Totals::default(),
        }
    }

    fn log(&self, line: impl FnOnce() -> String) {
        if self.trace {
            println!("[{}] {}", self.seed, line());
        }
    }

    async fn run(mut self) -> Result<Totals> {
        self.log(|| self.config.summary.clone());
        let mut open = self.open().await?;
        let tables = "CREATE TABLE t0(id INTEGER PRIMARY KEY, n INTEGER, body TEXT); CREATE TABLE t1(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)";
        self.model.run(tables).unwrap();
        match &mut open {
            Open::Session(session) => drop(session.query(tables, None).await?),
            Open::Turns { sql, .. } => sql.execute(tables).await?,
        }
        if self.config.session {
            self.model.acknowledge();
        }
        while self.totals.calls < STEPS {
            open = match self.config.session {
                true => self.session_step(open).await?,
                false => self.turn(open).await?,
            };
        }
        drop(open);
        self.end(false, "the end").await?;
        Ok(self.totals)
    }

    /// Open the next session, through a fault at open now and then (which
    /// writes nothing); it sees exactly the state the model holds.
    async fn open(&mut self) -> Result<Open> {
        self.totals.reopens += 1;
        self.in_body = false;
        if let Some(plan) = self.fault(4, &[Operation::Get]) {
            let writes = self.writes();
            let (_, ended) = under(&self.store, plan, self.open_once()).await;
            assert_eq!(self.writes(), writes, "seed {}: open wrote", self.seed);
            if let Ended::Returned(Ok(mut open)) = ended {
                self.check_view(&mut open).await?;
                return Ok(open);
            }
        }
        let mut open = self.open_once().await?;
        self.check_view(&mut open).await?;
        Ok(open)
    }

    async fn open_once(&self) -> Result<Open> {
        let store: Arc<dyn KvStore> = self.store.clone();
        let options = self.config.options.clone();
        Ok(match self.config.session {
            true => Open::Session(Session::new(options.open(store).await?).await?),
            false => {
                let state = options.open(store).await?;
                let sql = Sql::open(&state).await?;
                Open::Turns { state, sql }
            }
        })
    }

    async fn check_view(&mut self, open: &mut Open) -> Result<()> {
        let seen = open.dump().await?;
        assert!(
            seen == self.model.acknowledged_state(),
            "seed {}: a new session sees another state than the flushed one",
            self.seed
        );
        Ok(())
    }

    /// Puts and deletes issued so far.
    fn writes(&self) -> u64 {
        let stats = self.memory.stats();
        stats.puts + stats.deletes
    }

    /// A storage fault for the next call, one time in `one_in`: mostly at the first
    /// request of a flush (which every call that flushes makes), else at
    /// any request.
    fn fault(&mut self, one_in: u64, operations: &[Operation]) -> Option<Failpoint> {
        if !self.rng.chance(one_in) {
            return None;
        }
        let operation = self.rng.pick(operations);
        let prefixes: &[&str] = match operation {
            Operation::Put => &["", "", "", HEAD_KEY, TAIL_PREFIX, KV_PREFIX],
            Operation::Get => &["", HEAD_KEY, TAIL_PREFIX, KV_PREFIX],
            Operation::Delete => &["", TAIL_PREFIX, KV_PREFIX],
            _ => &[""],
        };
        Some(Failpoint {
            operation,
            key_prefix: self.rng.pick(prefixes).into(),
            nth: self.rng.pick(&[1, 1, 1, 2, 3]),
            effect: self.rng.pick(&[Effect::None, Effect::After]),
            cancel: self.rng.chance(3),
        })
    }

    /// Run `call`, under a fault one time in `one_in`: whether the fault was hit, and
    /// how the call ended.
    async fn call<T>(
        &mut self,
        one_in: u64,
        what: &str,
        call: impl Future<Output = Result<T>>,
    ) -> (bool, Ended<T>) {
        self.totals.calls += 1;
        let plan = self.fault(
            one_in,
            &[
                Operation::Get,
                Operation::Put,
                Operation::Put,
                Operation::Put,
                Operation::Delete,
                Operation::Sync,
                Operation::Sync,
                Operation::Sync,
                Operation::Keys,
            ],
        );
        self.log(|| format!("{what} {plan:?}"));
        let (hit, ended) = match plan {
            Some(plan) => under(&self.store, plan, call).await,
            None => (false, Ended::Returned(call.await)),
        };
        self.log(|| {
            format!(
                "  requests {:?}",
                self.store
                    .events()
                    .iter()
                    .map(|e| format!(
                        "{:?} {}{}",
                        e.operation,
                        e.key,
                        if e.faulted { " FAULT" } else { "" }
                    ))
                    .collect::<Vec<_>>()
            )
        });
        if hit {
            self.totals.faults += 1;
        }
        (hit, ended)
    }

    /// The session ended (it was dropped): crash now and then, keeping any
    /// prefix of the writes not yet synced, then the store holds the state
    /// the model acknowledged or, if `unknown`, the one pending.
    async fn end(&mut self, unknown: bool, case: &str) -> Result<()> {
        let case = format!(
            "seed {} ({}), call {}: {case}",
            self.seed, self.config.summary, self.totals.calls
        );
        assert_eq!(self.store.violations(), Vec::<String>::new(), "{case}");
        assert_eq!(self.store.active(), 0, "{case}: requests settle");
        let unsynced = self.memory.unsynced().len();
        let mut lost = false;
        if unsynced > 0 || self.rng.chance(3) {
            let kept = self.rng.below(unsynced as u64 + 1) as usize;
            self.log(|| format!("crash keeping {kept} of {unsynced} writes"));
            self.memory.crash_keeping(kept);
            self.totals.crashes += 1;
            lost = kept < unsynced;
        }
        // A crash of the process takes the cache with it; a store that lost
        // writes while the process lives keeps it.
        if self.config.cached && (lost || self.rng.chance(4)) && self.rng.chance(2) {
            self.config.options.cache = Some(fresh_namespace());
        }
        if unknown {
            self.totals.unknown_outcomes += 1;
        }
        let seen = observe(self.memory.clone()).await?;
        self.model.settle(&seen, unknown, &case);
        self.store = Arc::new(MonitorKv::seeded(
            FaultKv::new(self.memory.clone(), self.seed),
            self.memory.values(),
        ));
        Ok(())
    }

    /// One call of the bindings' session.
    async fn session_step(&mut self, open: Open) -> Result<Open> {
        let Open::Session(session) = open else {
            unreachable!()
        };
        let writes = self.writes();
        let mut ended_body = false;
        let (hit, ended, expected) = match self.rng.below(10) {
            0..=5 => {
                // A body must not end its transaction.
                let sql = match self.in_body {
                    true => statement(&mut self.rng),
                    false => top_level(&mut self.rng),
                };
                let expected = self.model.run(&sql).map(|rows| text(&rows));
                let (hit, ended) = self.call(4, &sql, session.query(&sql, None)).await;
                (hit, ended.map(|(_, rows)| text(&rows)), Some(expected))
            }
            6 | 7 => {
                let expected = self.model.run("BEGIN").map(drop);
                let (hit, ended) = self.call(4, "begin", session.begin()).await;
                self.in_body |= matches!(ended, Ended::Returned(Ok(())));
                (
                    hit,
                    ended.map(|()| Vec::new()),
                    Some(expected.map(|()| Vec::new())),
                )
            }
            8 => {
                let effects = Bytes::from(format!("effects {}", self.totals.calls));
                let expected = match self.model.in_transaction() {
                    true => self.model.run("COMMIT").map(drop),
                    false => Ok(()),
                };
                if expected.is_ok() {
                    self.model.pending_effects = Some(effects.clone());
                }
                self.in_body = false;
                let (hit, ended) = self.call(3, "commit", session.commit(Some(effects))).await;
                (
                    hit,
                    ended.map(|()| Vec::new()),
                    Some(expected.map(|()| Vec::new())),
                )
            }
            _ => match self.rng.below(3) {
                0 => {
                    if self.model.in_transaction() {
                        self.model.run("ROLLBACK").unwrap();
                    }
                    self.in_body = false;
                    let (hit, ended) = self.call(4, "rollback", session.rollback()).await;
                    (hit, ended.map(|()| Vec::new()), Some(Ok(Vec::new())))
                }
                1 => {
                    // The end of the session: clean unless a transaction is open.
                    self.log(|| "save".into());
                    let saved = session.save().await;
                    assert_eq!(
                        saved.is_ok(),
                        !self.model.in_transaction(),
                        "seed {}: save",
                        self.seed
                    );
                    drop(session);
                    self.end(false, "save").await?;
                    return self.open().await;
                }
                _ => {
                    // A body that ends its transaction ends the session.
                    if self.in_body {
                        ended_body = true;
                        let sql = self.rng.pick(&["COMMIT", "ROLLBACK", "END"]);
                        self.model.run(sql).unwrap();
                        let (hit, ended) = self.call(4, sql, session.query(sql, None)).await;
                        (hit, ended.map(|_| Vec::new()), None)
                    } else {
                        self.log(|| "close".into());
                        session.close().await;
                        drop(session);
                        self.end(false, "close").await?;
                        return self.open().await;
                    }
                }
            },
        };
        let open = Open::Session(session);
        self.settle_call(open, writes, hit, ended, expected, ended_body)
            .await
    }

    /// After a call: a session the call ended ends here, and a usable one
    /// gave the reference engine's answer, and acknowledged what it
    /// committed.
    async fn settle_call(
        &mut self,
        open: Open,
        writes: u64,
        hit: bool,
        ended: Ended<Vec<String>>,
        expected: Option<std::result::Result<Vec<String>, String>>,
        deliberate: bool,
    ) -> Result<Open> {
        let case = format!("seed {}, call {}", self.seed, self.totals.calls);
        if !open.is_usable() {
            assert!(
                hit || deliberate,
                "{case}: the session ended without a fault: {:?}",
                ended.error()
            );
            drop(open);
            let unknown = self.writes() > writes;
            self.end(unknown, "the session ended").await?;
            return self.open().await;
        }
        assert!(
            !matches!(ended, Ended::Cancelled),
            "{case}: usable after a cancelled call"
        );
        assert!(
            !deliberate,
            "{case}: usable after its body ended its transaction"
        );
        let Ended::Returned(got) = ended else {
            unreachable!()
        };
        if let Some(expected) = expected {
            match (&got, &expected) {
                (Ok(got), Ok(expected)) => assert_eq!(got, expected, "{case}"),
                (Err(_), Err(_)) => {}
                _ => panic!("{case}: {got:?}, the reference engine: {expected:?}"),
            }
        }
        if !self.model.in_transaction() {
            match &open {
                // The call flushed before it returned.
                Open::Session(_) => self.model.acknowledge(),
                Open::Turns { .. } => {}
            }
        }
        Ok(open)
    }

    /// One call of a turn of `Sql`: a statement, a transaction, or the
    /// turn's flush; or the end of the session.
    async fn turn(&mut self, open: Open) -> Result<Open> {
        let Open::Turns { state, mut sql } = open else {
            unreachable!()
        };
        let writes = self.writes();
        let (hit, ended, expected) = match self.rng.below(8) {
            0..=3 => {
                let sql_text = top_level(&mut self.rng);
                let expected = self.model.run(&sql_text).map(|rows| text(&rows));
                let (hit, ended) = self.call(4, &sql_text, sql.query(&sql_text)).await;
                (hit, ended.map(|rows| text(&rows)), Some(expected))
            }
            4 => {
                let statements: Vec<String> = (0..1 + self.rng.below(3))
                    .map(|_| statement(&mut self.rng))
                    .collect();
                let commits = !self.rng.chance(3);
                let expected = match self.model.run("BEGIN") {
                    Ok(_) => {
                        for statement in &statements {
                            let _ = self.model.run(statement);
                        }
                        let end = if commits { "COMMIT" } else { "ROLLBACK" };
                        self.model.run(end).unwrap();
                        match commits {
                            true => Ok(Vec::new()),
                            false => Err("rolled back".to_owned()),
                        }
                    }
                    Err(error) => Err(error),
                };
                let body = async |sql: &mut Sql| {
                    for statement in &statements {
                        let _ = sql.execute(statement).await;
                    }
                    match commits {
                        true => Ok(()),
                        false => Err("rolled back".into()),
                    }
                };
                let what = format!("transaction {statements:?}, commits: {commits}");
                let (hit, ended) = self.call(4, &what, sql.transaction(body)).await;
                (hit, ended.map(|()| Vec::new()), Some(expected))
            }
            5 | 6 => {
                self.turn += 1;
                if self.model.in_transaction() {
                    self.model.run("COMMIT").unwrap();
                }
                let effects = Bytes::from(format!("turn {}", self.turn));
                self.model.pending_effects = Some(effects.clone());
                let flush = async {
                    let mut mutation = state.mutation();
                    sql.flush(&mut mutation).await?;
                    mutation.set(&state.record("effects"), effects)?;
                    mutation.flush().await.map(drop)
                };
                let (hit, ended) = self.call(2, "flush the turn", flush).await;
                if matches!(ended, Ended::Returned(Ok(()))) {
                    self.model.acknowledge();
                }
                (hit, ended.map(|()| Vec::new()), None)
            }
            _ => {
                self.log(|| "drop the session".into());
                drop(sql);
                drop(state);
                self.end(false, "dropped").await?;
                return self.open().await;
            }
        };
        let open = Open::Turns { state, sql };
        self.settle_call(open, writes, hit, ended, expected, false)
            .await
    }
}

impl<T> Ended<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Ended<U> {
        match self {
            Ended::Returned(result) => Ended::Returned(result.map(f)),
            Ended::Cancelled => Ended::Cancelled,
        }
    }

    fn error(&self) -> Option<String> {
        match self {
            Ended::Returned(Err(error)) => Some(error.to_string()),
            _ => None,
        }
    }
}

/// The state of the store and its effects, through a session without the
/// cache; its database checks out.
async fn observe(memory: MemoryKv) -> Result<(String, Bytes)> {
    let state = State::open(Arc::new(memory)).await?;
    let mut sql = Sql::open(&state).await?;
    assert_eq!(integrity(&mut sql).await?, "ok");
    Ok((dump(&mut sql).await?, state.record("effects").get()))
}

/// A value of `size` characters, past a page or several when large.
fn body(rng: &mut Rng) -> String {
    match rng.pick(&[0, 10, 300, 1500, 4000, 9000, 20000]) {
        0 => "''".into(),
        size => format!("printf('%0{size}d', {})", rng.below(1000)),
    }
}

/// A statement of the workload, which may fail.
fn statement(rng: &mut Rng) -> String {
    let t = format!("t{}", rng.below(3));
    let id = |rng: &mut Rng| rng.below(40);
    match rng.below(22) {
        0 => format!("CREATE TABLE {t}(id INTEGER PRIMARY KEY, n INTEGER, body TEXT)"),
        1 => format!("CREATE INDEX {t}_n ON {t}(n)"),
        2 => format!("CREATE INDEX {t}_body ON {t}(body)"),
        3 => format!("DROP INDEX {t}_{}", rng.pick(&["n", "body"])),
        4 if rng.chance(4) => format!("DROP TABLE {t}"),
        5..=8 => format!(
            "INSERT INTO {t}(id, n, body) VALUES({}, {}, {})",
            id(rng),
            rng.below(9),
            body(rng)
        ),
        9 => format!(
            "INSERT INTO {t}(id, n, body) VALUES({}, 1, {}), ({}, 2, {})",
            id(rng),
            body(rng),
            id(rng),
            body(rng)
        ),
        10 => format!(
            "INSERT OR REPLACE INTO {t}(id, n, body) VALUES({}, 3, {})",
            id(rng),
            body(rng)
        ),
        11 | 12 => format!(
            "UPDATE {t} SET n = n + {}, body = {} WHERE id % {} = {}",
            rng.below(5),
            body(rng),
            2 + rng.below(4),
            rng.below(2)
        ),
        13 => {
            let from = id(rng);
            format!(
                "DELETE FROM {t} WHERE id BETWEEN {from} AND {}",
                from + rng.below(10)
            )
        }
        14 => format!("SELECT count(*), sum(n), sum(length(body)) FROM {t}"),
        15 => format!(
            "SELECT id, n, length(body), substr(body, -4) FROM {t} WHERE n > {} ORDER BY id",
            rng.below(5)
        ),
        16 => "VACUUM".into(),
        17 => format!(
            "ALTER TABLE {t} ADD COLUMN c{} INTEGER DEFAULT 7",
            rng.below(3)
        ),
        18 => format!("SELECT count(*) FROM {t} WHERE body > '{}'", rng.below(10)),
        _ => format!("UPDATE {t} SET n = -n WHERE id = {}", id(rng)),
    }
}

/// SQL of a call outside a body: a statement; a string of them that fails
/// partway; or transaction control, as statements or in one string.
fn top_level(rng: &mut Rng) -> String {
    match rng.below(14) {
        0 => format!(
            "{}; INSERT INTO missing VALUES(1); {}",
            statement(rng),
            statement(rng)
        ),
        1 => format!("BEGIN; {}; {}; COMMIT", statement(rng), statement(rng)),
        2 => rng.pick(&["BEGIN", "COMMIT", "ROLLBACK"]).into(),
        3 => rng
            .pick(&["SAVEPOINT s", "RELEASE s", "ROLLBACK TO s"])
            .into(),
        4 => format!("{}; {}", statement(rng), statement(rng)),
        _ => statement(rng),
    }
}

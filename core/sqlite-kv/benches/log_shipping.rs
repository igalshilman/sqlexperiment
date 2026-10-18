//! The WAL-native store in critical-path round trips.
//!
//! Each configuration replays the same interleaved session sequence: a point
//! read, a one-row update, a ten-row update and an insert, 32 times. Every
//! session reopens; with `cache`, all sessions of a run share one `PageCache`
//! as a long-lived owner process would. The backend is strong: a read returns
//! only durable values. Round trips come from `LatencyKv`'s virtual clock;
//! operation and byte counts from `MemoryKv`. Output is CSV.
use sqlite_kv::{
    NamespaceCache, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv, cache_config, open_cache},
};
use std::sync::Arc;

#[derive(Clone, Copy)]
struct Case {
    rows: u64,
    payload: usize,
    indexes: usize,
}
const CASES: [Case; 2] = [
    Case {
        rows: 100,
        payload: 64,
        indexes: 0,
    },
    Case {
        rows: 10_000,
        payload: 256,
        indexes: 1,
    },
];
/// When saves checkpoint: once the log holds more than `after` transactions or
/// `log_bytes` bytes; and how much of it the record holds inline.
#[derive(Clone, Copy)]
struct Policy {
    after: u32,
    log_bytes: usize,
    inline_bytes: usize,
}
/// The default (4 transactions), 16 transactions, and the bindings' cadence:
/// 16 transactions or 16 KiB. `POLICIES` overrides them:
/// `after:log_bytes[:inline_bytes]`, comma-separated.
const POLICIES: [Policy; 3] = [
    Policy {
        after: 4,
        log_bytes: sqlite_kv::CHECKPOINT_LOG_BYTES,
        inline_bytes: sqlite_kv::INLINE_LOG_BYTES,
    },
    Policy {
        after: 16,
        log_bytes: sqlite_kv::CHECKPOINT_LOG_BYTES,
        inline_bytes: sqlite_kv::INLINE_LOG_BYTES,
    },
    Policy {
        after: 16,
        log_bytes: 16 * 1024,
        inline_bytes: sqlite_kv::INLINE_LOG_BYTES,
    },
];

fn policies() -> Vec<Policy> {
    let Ok(list) = std::env::var("POLICIES") else {
        return POLICIES.to_vec();
    };
    list.split(',')
        .map(|policy| {
            let mut fields = policy
                .split(':')
                .map(|field| field.parse().expect("a number"));
            let after = fields.next().expect("after") as u32;
            let log_bytes = fields.next().expect("log bytes");
            Policy {
                after,
                log_bytes,
                inline_bytes: fields.next().unwrap_or(sqlite_kv::INLINE_LOG_BYTES),
            }
        })
        .collect()
}
const ITERATIONS: u64 = 32;
const WORKLOADS: [&str; 4] = ["read_pk", "update_1", "update_10", "insert_1"];

type Store = Arc<LatencyKv<MemoryKv>>;

fn options(policy: Policy, cache: &Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        checkpoint_after: policy.after,
        checkpoint_log_bytes: policy.log_bytes,
        inline_log_bytes: policy.inline_bytes,
        cache: cache.clone(),
        ..StorageOptions::default()
    }
}

async fn baseline(case: Case) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let kv = Arc::new(store.clone());
    let mut db = Session::open(kv.clone()).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, payload TEXT)")
        .await?;
    if case.indexes > 0 {
        db.execute("CREATE INDEX by_a ON t(a)").await?;
    }
    db.execute(&format!(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{}) \
         INSERT INTO t SELECT x, x, printf('%0{}d', x) FROM n",
        case.rows, case.payload
    ))
    .await?;
    db.save().await?;
    // Steady state: a later save checkpoints the bulk load into page keys.
    let mut db = Session::open_with_options(
        kv,
        options(
            Policy {
                after: 0,
                log_bytes: 0,
                inline_bytes: 0,
            },
            &None,
        ),
    )
    .await?;
    db.execute("CREATE TABLE other(x)").await?;
    db.save().await?;
    Ok(store)
}

/// Deterministic scattered row IDs.
fn row(case: Case, iteration: u64, n: u64) -> u64 {
    (iteration * 7919 + n * 104_729) % case.rows + 1
}

struct Phase<'a> {
    case: Case,
    policy: Policy,
    strong: bool,
    cache: &'a Option<NamespaceCache>,
    run: usize,
    iteration: u64,
    workload: &'a str,
}
impl Phase<'_> {
    fn print(&self, phase: &str, store: &Store) {
        let s = store.inner.stats();
        let segments = store
            .inner
            .keys()
            .iter()
            .filter(|k| k.contains("/log/"))
            .count();
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{phase},{},{},{},{},{},{},{segments},{},{},{},{}",
            self.case.rows,
            self.case.payload,
            self.case.indexes,
            self.policy.after,
            self.policy.log_bytes,
            self.policy.inline_bytes,
            if self.strong { "strong" } else { "weak" },
            self.cache.is_some(),
            self.run,
            self.iteration,
            self.workload,
            store.round_trips(),
            s.gets,
            s.puts,
            s.deletes,
            s.bytes_read,
            s.bytes_written,
            store.inner.stored_bytes(),
            s.kind("commit").bytes_written,
            s.kind("page").bytes_written,
            s.kind("log").bytes_written,
        );
    }
}
fn reset(store: &Store) {
    store.reset();
    store.inner.reset_stats();
}

async fn session(store: &Store, phase: &Phase<'_>) -> Result<()> {
    let Phase {
        case, iteration, ..
    } = *phase;
    reset(store);
    let kv: Arc<dyn KvStore> = store.clone();
    let mut db = Session::open_with_options(kv, options(phase.policy, phase.cache)).await?;
    phase.print("open", store);
    reset(store);
    match phase.workload {
        "read_pk" => {
            let id = row(case, iteration, 0);
            let rows = db
                .query(&format!("SELECT length(payload) FROM t WHERE id = {id}"))
                .await?;
            assert!(rows.len() <= 1);
        }
        "update_1" => {
            let id = row(case, iteration, 1);
            db.execute(&format!("UPDATE t SET a = a + 1 WHERE id = {id}"))
                .await?;
        }
        "update_10" => {
            db.execute("BEGIN").await?;
            for n in 0..10 {
                let id = row(case, iteration, n + 2);
                db.execute(&format!("UPDATE t SET a = a + 1 WHERE id = {id}"))
                    .await?;
            }
            db.execute("COMMIT").await?;
        }
        "insert_1" => {
            let id = case.rows + iteration + 1;
            db.execute(&format!(
                "INSERT INTO t VALUES({id}, {id}, printf('%0{}d', {id}))",
                case.payload
            ))
            .await?;
        }
        _ => unreachable!(),
    }
    phase.print("query", store);
    if phase.workload != "read_pk" {
        reset(store);
        db.save().await?;
        phase.print("save", store);
    }
    Ok(())
}

async fn run(case: Case, policy: Policy, strong: bool, cached: bool, run: usize) -> Result<()> {
    let store: Store = Arc::new(LatencyKv::new(baseline(case).await?));
    let cache = cached.then(|| open_cache(cache_config(256 << 20, 1 << 30)).namespace("bench"));
    // Warm the owner state outside the measurement.
    let mut db = Session::open_with_options(store.clone(), options(policy, &cache)).await?;
    db.execute("UPDATE t SET a = a WHERE id = 1").await?;
    db.save().await?;
    for iteration in 0..ITERATIONS {
        for workload in WORKLOADS {
            let phase = Phase {
                case,
                policy,
                strong,
                cache: &cache,
                run,
                iteration,
                workload,
            };
            session(&store, &phase).await?;
        }
    }
    // Every write must be visible to a publish-every-save reader.
    let mut db = Session::open(store.clone()).await?;
    let rows = db.query("SELECT count(*), sum(a) FROM t").await?;
    assert_eq!(rows[0][0].as_int(), Some((case.rows + ITERATIONS) as i64));
    Ok(())
}

fn main() -> Result<()> {
    let repeats = std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .map_or(Ok(1), |arg| arg.parse())?;
    println!(
        "rows,payload_bytes,indexes,checkpoint_after,checkpoint_log_bytes,inline_log_bytes,backend,cache,run,iteration,workload,phase,round_trips,gets,puts,deletes,bytes_read,bytes_written,log_segments,stored_bytes,commit_written,page_written,log_written"
    );
    futures::executor::block_on(async {
        for case in CASES {
            for &policy in &policies() {
                // A read returns only durable values: the backend is strong.
                for strong in [true] {
                    for cached in [false, true] {
                        for repeat in 0..repeats {
                            run(case, policy, strong, cached, repeat).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

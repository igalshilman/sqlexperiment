//! Publish-every-save versus the durable delta log, in critical-path round trips.
//!
//! Each configuration replays the same interleaved session sequence: a point
//! read, a one-row update, a ten-row update and an insert, 32 times. Every
//! session reopens; with `cache`, all sessions of a run share one `ObjectCache`
//! as a long-lived owner process would. Round trips come from `LatencyKv`'s
//! virtual clock; operation and byte counts from `MemoryKv`. Output is CSV.
use sqlite_kv::{
    ObjectCache, Result, Session, StorageOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv},
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
/// Zero publishes on every save; others allow that many log segments.
const POLICIES: [u32; 3] = [0, 4, 16];
const ITERATIONS: u64 = 32;
const WORKLOADS: [&str; 4] = ["read_pk", "update_1", "update_10", "insert_1"];

type Store = Arc<LatencyKv<MemoryKv>>;

fn options(policy: u32, cache: &Option<ObjectCache>) -> StorageOptions {
    StorageOptions {
        max_log_segments: policy,
        cache: cache.clone(),
        ..StorageOptions::default()
    }
}

async fn baseline(case: Case) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db = Session::open(Arc::new(store.clone())).await?;
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
    Ok(store)
}

/// Deterministic scattered row IDs.
fn row(case: Case, iteration: u64, n: u64) -> u64 {
    (iteration * 7919 + n * 104_729) % case.rows + 1
}

struct Phase<'a> {
    case: Case,
    policy: u32,
    cache: &'a Option<ObjectCache>,
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
            "{},{},{},{},{},{},{},{},{phase},{},{},{},{},{},{},{segments},{}",
            self.case.rows,
            self.case.payload,
            self.case.indexes,
            self.policy,
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

async fn run(case: Case, policy: u32, cached: bool, run: usize) -> Result<()> {
    let store: Store = Arc::new(LatencyKv::new(baseline(case).await?));
    let cache = cached.then(|| ObjectCache::new(256 << 20));
    // Enable the log outside the measurement; the first logged save folds.
    let mut db = Session::open_with_options(store.clone(), options(policy, &cache)).await?;
    db.execute("UPDATE t SET a = a WHERE id = 1").await?;
    db.save().await?;
    for iteration in 0..ITERATIONS {
        for workload in WORKLOADS {
            let phase = Phase {
                case,
                policy,
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
        "rows,payload_bytes,indexes,max_log_segments,cache,run,iteration,workload,phase,round_trips,gets,puts,deletes,bytes_read,bytes_written,log_segments,stored_bytes"
    );
    futures::executor::block_on(async {
        for case in CASES {
            for policy in POLICIES {
                for cached in [false, true] {
                    for repeat in 0..repeats {
                        run(case, policy, cached, repeat).await?;
                    }
                }
            }
        }
        Ok(())
    })
}

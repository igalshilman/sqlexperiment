//! Copy-on-write blobs on SQL workloads, one flush per session, and a sweep
//! every `sweep_every` versions: the requests, bytes and round trips a flush
//! costs (its share of the sweeps included), what is stored, and a cold read.
//!
//! ```sh
//! cargo bench --locked --features simulation --bench flush_costs
//! CHUNK_PAGES=1 REBASE_RATIO=0.5 cargo bench --locked --features simulation --bench flush_costs
//! ```
//!
//! `CHUNK_PAGES`, `FANOUT`, `REBASE_RATIO`, `INLINE_BYTES`, `GARBAGE_AFTER` and
//! `SWEEP_EVERY` set the state's options; `WORKLOAD` runs only the workloads it names part of,
//! `FLUSHES` that many flushes each (an aged database); `LABEL` names the run
//! in its rows. Prints Markdown rows.
//!
//! With `S3=1`, the blobs are kept as beside Restate, in an object store (so
//! each session claims its numbers first), and the rows are what S3 would
//! bill: requests and bytes per flush, and dollars per million flushes and
//! per month stored, at S3 Standard's and S3 Express One Zone's prices.
use sql::{Sql, Value};
use state::{
    Result, State, StateOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv, Namespace, cache_config, object_kind, open_cache},
};
use std::sync::Arc;

type Store = Arc<LatencyKv<MemoryKv>>;

/// Deterministic text: words from a vocabulary of made-up words.
struct Words {
    seed: u64,
    vocabulary: Vec<String>,
}

impl Words {
    fn new() -> Self {
        let mut words = Self {
            seed: 0x9e37_79b9_7f4a_7c15,
            vocabulary: Vec::new(),
        };
        const SYLLABLES: [&str; 24] = [
            "ka", "ro", "mi", "tel", "an", "us", "ver", "do", "sta", "ne", "pri", "lo", "qua",
            "en", "tor", "ia", "be", "sol", "ux", "re", "mon", "ti", "gra", "el",
        ];
        for _ in 0..2000 {
            let n = 1 + words.next() % 3;
            let word: String = (0..n)
                .map(|_| SYLLABLES[(words.next() % 24) as usize])
                .collect();
            words.vocabulary.push(word);
        }
        words
    }

    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    /// About `len` bytes of words.
    fn text(&mut self, len: usize) -> String {
        let mut text = String::new();
        while text.len() < len {
            // Zipf-ish: common words more often.
            let r = self.next() % 2000;
            let index = (r * r / 2000) as usize;
            text.push_str(&self.vocabulary[index]);
            text.push(' ');
        }
        text.truncate(len);
        text
    }
}

fn options() -> StateOptions {
    fn var<T: std::str::FromStr>(name: &str) -> Option<T> {
        std::env::var(name).ok().and_then(|v| v.parse().ok())
    }
    let default = StateOptions::default();
    StateOptions {
        chunk_pages: var("CHUNK_PAGES").unwrap_or(default.chunk_pages),
        fanout: var("FANOUT").unwrap_or(default.fanout),
        rebase_ratio: var("REBASE_RATIO").unwrap_or(default.rebase_ratio),
        inline_record_bytes: var("INLINE_BYTES").unwrap_or(default.inline_record_bytes),
        garbage_after: var("GARBAGE_AFTER").unwrap_or(default.garbage_after),
        sweep_every: var("SWEEP_EVERY").unwrap_or(default.sweep_every),
        ..default
    }
}

async fn open(store: &Store, cache: &Option<Namespace>) -> Result<Sql> {
    let kv: Arc<dyn KvStore> = store.clone();
    let open = State::open(kv).options(options());
    let state = match cache {
        Some(ns) => open.cached(&ns.cache, &ns.key).await?,
        None => open.await?,
    };
    Sql::open(&state).await
}

/// Flush the SQL, and `effects` as the record `effects`, in one mutation.
async fn save(mut db: Sql, effects: Option<String>) -> Result<()> {
    let state = db.state();
    let mut mutation = state.mutation();
    db.flush(&mut mutation).await?;
    if let Some(effects) = effects {
        mutation.set(&state.record("effects"), effects.into())?;
    }
    mutation.flush().await?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Workload {
    name: &'static str,
    rows: u64,
    payload: usize,
    flushes: u64,
}

const WORKLOADS: [Workload; 10] = [
    // A restate-fns object: a message row, and its sends as effects.
    Workload {
        name: "chat+effects",
        rows: 0,
        payload: 400,
        flushes: 300,
    },
    Workload {
        name: "small: update 1",
        rows: 100,
        payload: 64,
        flushes: 300,
    },
    Workload {
        name: "10k: update 1",
        rows: 10_000,
        payload: 256,
        flushes: 200,
    },
    Workload {
        name: "10k: update 10",
        rows: 10_000,
        payload: 256,
        flushes: 200,
    },
    Workload {
        name: "10k: insert 1",
        rows: 10_000,
        payload: 256,
        flushes: 200,
    },
    Workload {
        name: "100k: update 1",
        rows: 100_000,
        payload: 256,
        flushes: 100,
    },
    // 56 MB: the chunk map is two levels deep.
    Workload {
        name: "200k: update 1",
        rows: 200_000,
        payload: 256,
        flushes: 100,
    },
    Workload {
        name: "200k: update 10",
        rows: 200_000,
        payload: 256,
        flushes: 100,
    },
    // 280 MB: three levels.
    Workload {
        name: "1M: update 1",
        rows: 1_000_000,
        payload: 256,
        flushes: 50,
    },
    Workload {
        name: "1M: update 10",
        rows: 1_000_000,
        payload: 256,
        flushes: 50,
    },
];

async fn load(store: &Store, w: Workload, words: &mut Words) -> Result<()> {
    let mut db = open(store, &None).await?;
    if w.name.starts_with("chat") {
        db.execute("CREATE TABLE messages(id INTEGER PRIMARY KEY, role TEXT, text TEXT)")
            .await?;
        return save(db, None).await;
    }
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, payload TEXT)")
        .await?;
    db.execute("CREATE INDEX by_a ON t(a)").await?;
    // A session of 100k rows at a time, within its private changes' limit.
    for batch in (1..=w.rows).collect::<Vec<_>>().chunks(100_000) {
        db.execute("BEGIN").await?;
        for &id in batch {
            db.execute_with(
                "INSERT INTO t VALUES(?, ?, ?)",
                [
                    Value::from_i64(id as i64),
                    Value::from_i64(id as i64),
                    Value::build_text(words.text(w.payload)),
                ],
            )
            .await?;
        }
        db.execute("COMMIT").await?;
        save(db, None).await?;
        db = open(store, &None).await?;
    }
    save(db, None).await
}

fn row(w: Workload, i: u64, n: u64) -> i64 {
    ((i * 7919 + n * 104_729) % w.rows + 1) as i64
}

async fn step(db: &mut Sql, w: Workload, i: u64, words: &mut Words) -> Result<Option<String>> {
    match w.name {
        "chat+effects" => {
            let text = words.text(w.payload);
            db.execute_with(
                "INSERT INTO messages(role, text) VALUES(?, ?)",
                [
                    Value::build_text(if i.is_multiple_of(2) {
                        "user"
                    } else {
                        "assistant"
                    }),
                    Value::build_text(text.clone()),
                ],
            )
            .await?;
            let effects = format!(
                r#"{{"sends":[{{"service":"Notifier","handler":"message","key":"session-17","idempotencyKey":"inv_1{i:015x}-{i}","input":{{"id":{i},"text":"{text}"}}}}]}}"#
            );
            Ok(Some(effects))
        }
        name if name.ends_with("update 1") => {
            db.execute_with(
                "UPDATE t SET a = a + 1, payload = ? WHERE id = ?",
                [
                    Value::build_text(words.text(w.payload)),
                    Value::from_i64(row(w, i, 0)),
                ],
            )
            .await?;
            Ok(None)
        }
        name if name.ends_with("update 10") => {
            db.execute("BEGIN").await?;
            for n in 0..10 {
                db.execute_with(
                    "UPDATE t SET a = a + 1 WHERE id = ?",
                    [Value::from_i64(row(w, i, n))],
                )
                .await?;
            }
            db.execute("COMMIT").await?;
            Ok(None)
        }
        "10k: insert 1" => {
            let id = (w.rows + i + 1) as i64;
            db.execute_with(
                "INSERT INTO t VALUES(?, ?, ?)",
                [
                    Value::from_i64(id),
                    Value::from_i64(id),
                    Value::build_text(words.text(w.payload)),
                ],
            )
            .await?;
            Ok(None)
        }
        _ => unreachable!(),
    }
}

/// Prices in US East, in dollars: S3 Standard, then S3 Express One Zone.
const PUT: [f64; 2] = [0.005 / 1000.0, 0.00113 / 1000.0];
const UPLOAD_GB: [f64; 2] = [0.0, 0.0032];
const STORED_GB_MONTH: [f64; 2] = [0.023, 0.11];
const GB: f64 = (1u64 << 30) as f64;

fn s3() -> bool {
    std::env::var_os("S3").is_some()
}

async fn run(w: Workload, cached: bool) -> Result<()> {
    let memory = match s3() {
        true => MemoryKv::unfenced(),
        false => MemoryKv::default(),
    };
    let store: Store = Arc::new(LatencyKv::new(memory.clone()));
    let mut words = Words::new();
    load(&store, w, &mut words).await?;
    let cache =
        cached.then(|| Namespace::new(&open_cache(cache_config(256 << 20, 1 << 30)), "bench"));
    // Warm the owner, and its cache with every blob, outside the measurement.
    let mut db = open(&store, &cache).await?;
    if !w.name.starts_with("chat") {
        db.query("SELECT count(*), sum(length(payload)) FROM t")
            .await?;
        db.query("SELECT count(*) FROM t INDEXED BY by_a WHERE a > 0")
            .await?;
    }
    save(db, None).await?;
    store.reset();
    memory.reset_stats();
    for i in 0..w.flushes {
        let mut db = open(&store, &cache).await?;
        let effects = step(&mut db, w, i, &mut words).await?;
        save(db, effects).await?;
    }
    let s = memory.stats();
    let rts = store.round_trips();
    let n = w.flushes as f64;
    let stored = memory.stored_bytes() + memory.stored_key_bytes();
    let keys = memory.keys().len();
    // A cold open by a new owner, and a point read.
    store.reset();
    memory.reset_stats();
    let mut db = open(&store, &None).await?;
    let probe = match w.name {
        "chat+effects" => "SELECT text FROM messages WHERE id = 7",
        _ => "SELECT payload FROM t WHERE id = 4242",
    };
    db.query(probe).await?;
    let cold = memory.stats();
    let cold_rts = store.round_trips();
    let db_pages = db.query("PRAGMA page_count").await?[0][0]
        .as_int()
        .unwrap_or(0);
    drop(db);
    let label = std::env::var("LABEL").unwrap_or_else(|_| "cow".into());
    if s3() {
        // What goes to S3: blobs and the sweep's record; deletes are free.
        let (blob, sweep) = (s.kind("blob"), s.kind("sweep"));
        let puts = (blob.puts + sweep.puts) as f64 / n;
        let pages = s.list_pages as f64 / n;
        let upload = (blob.bytes_written + sweep.bytes_written) as f64 / n;
        let restate = s.kind("root").bytes_written + s.kind("claim").bytes_written;
        let blobs_stored: usize = (memory.contents().iter())
            .filter(|(key, _)| !matches!(object_kind(key), "root" | "claim"))
            .map(|(_, value)| value.len())
            .sum();
        let per_million =
            |tier: usize| 1e6 * ((puts + pages) * PUT[tier] + upload / GB * UPLOAD_GB[tier]);
        let cold_gets = cold.kind("blob").gets;
        println!(
            "| {} | {label} | {} | {:.2} | {:.2} | {:.3} | {:.0} | {:.0} | {:.2} | {:.1} | {cold_gets} | {:.2} | {:.2} | {:.4} | {:.4} |",
            w.name,
            if cached { "yes" } else { "no" },
            rts as f64 / n,
            puts,
            pages,
            upload,
            restate as f64 / n,
            s.kind("claim").puts as f64 / n,
            blobs_stored as f64 / 1024.0 / 1024.0,
            per_million(0),
            per_million(1),
            blobs_stored as f64 / GB * STORED_GB_MONTH[0],
            blobs_stored as f64 / GB * STORED_GB_MONTH[1],
        );
        return Ok(());
    }
    println!(
        "| {} | {label} | {} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} | {:.2} | {:.1} | {:.0} | {keys} | {} | {} | {} | {:.0} |",
        w.name,
        if cached { "yes" } else { "no" },
        rts as f64 / n,
        s.puts as f64 / n,
        s.bytes_written as f64 / n,
        s.kind("root").bytes_written as f64 / n,
        s.kind("blob").bytes_written as f64 / n,
        s.deletes as f64 / n,
        s.gets as f64 / n,
        stored as f64 / 1024.0,
        db_pages * 4,
        cold_rts,
        cold.gets,
        cold.bytes_read as f64 / 1024.0,
    );
    Ok(())
}

fn main() -> Result<()> {
    if s3() {
        println!(
            "| Workload | Run | Cache | RT/flush | S3 PUTs/flush | S3 LIST pages/flush | S3 upload B/flush | Restate B/flush | Claims/flush | S3 stored MiB | Cold S3 GETs | Standard $/M flushes | Express $/M flushes | Standard $/month stored | Express $/month stored |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    } else {
        println!(
            "| Workload | Run | Cache | RT/flush | Puts/flush | Put B/flush | Root B | Blob B | Deletes/flush | Gets/flush | Stored KiB | Keys | DB KiB | Cold RT | Cold gets | Cold read KiB |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    }
    let only = std::env::var("WORKLOAD").ok();
    let flushes = std::env::var("FLUSHES").ok().and_then(|v| v.parse().ok());
    futures::executor::block_on(async {
        for w in WORKLOADS {
            if only.as_deref().is_some_and(|only| !w.name.contains(only)) {
                continue;
            }
            let w = Workload {
                flushes: flushes.unwrap_or(w.flushes),
                ..w
            };
            for cached in [false, true] {
                run(w, cached).await?;
            }
        }
        Ok(())
    })
}

//! Copy-on-write blobs on SQL workloads, one flush per session: the requests,
//! bytes and round trips a flush costs, what is stored, and a cold read.
//!
//! ```sh
//! cargo bench --locked --features simulation --bench cow_compare
//! CHUNK_PAGES=1 REBASE_RATIO=0.5 cargo bench --locked --features simulation --bench cow_compare
//! ```
//!
//! `CHUNK_PAGES`, `REBASE_RATIO`, `INLINE_BYTES` and `GARBAGE_AFTER` set the
//! state's options; `LABEL` names the run in its rows. Prints Markdown rows.
use sql::{Sql, Value};
use state::{
    Result, State, StateOptions,
    kv::KvStore,
    simulation::{LatencyKv, MemoryKv, Namespace, cache_config, open_cache},
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
        rebase_ratio: var("REBASE_RATIO").unwrap_or(default.rebase_ratio),
        inline_record_bytes: var("INLINE_BYTES").unwrap_or(default.inline_record_bytes),
        garbage_after: var("GARBAGE_AFTER").unwrap_or(default.garbage_after),
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

/// Flush the SQL, and `effects` as the record `effects`, in one WAL.
async fn save(mut db: Sql, effects: Option<String>) -> Result<()> {
    let state = db.state();
    let mut wal = state.wal();
    db.flush(&mut wal).await?;
    if let Some(effects) = effects {
        wal.set(&state.record("effects"), effects.into())?;
    }
    wal.flush().await?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Workload {
    name: &'static str,
    rows: u64,
    payload: usize,
    flushes: u64,
}

const WORKLOADS: [Workload; 6] = [
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
    db.execute("BEGIN").await?;
    for id in 1..=w.rows {
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
        "small: update 1" | "10k: update 1" | "100k: update 1" => {
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
        "10k: update 10" => {
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

async fn run(w: Workload, cached: bool) -> Result<()> {
    let memory = MemoryKv::default();
    let store: Store = Arc::new(LatencyKv::new(memory.clone()));
    let mut words = Words::new();
    // TRACE_DIR: the cached run's flushes, for state's examples/replay.rs.
    let trace = std::env::var("TRACE_DIR")
        .ok()
        .filter(|_| cached)
        .map(|dir| format!("{dir}/{}.trace", w.name.replace([' ', ':', '+'], "_")));
    match &trace {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            unsafe { std::env::set_var("STATE_TRACE", path) }
        }
        None => unsafe { std::env::remove_var("STATE_TRACE") },
    }
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
    if let Some(path) = &trace {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"M").unwrap();
    }
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
    println!(
        "| Workload | Run | Cache | RT/flush | Puts/flush | Put B/flush | Root B | Blob B | Deletes/flush | Gets/flush | Stored KiB | Keys | DB KiB | Cold RT | Cold gets | Cold read KiB |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let only = std::env::var("WORKLOAD").ok();
    futures::executor::block_on(async {
        for w in WORKLOADS {
            if only.as_deref().is_some_and(|only| !w.name.contains(only)) {
                continue;
            }
            for cached in [false, true] {
                run(w, cached).await?;
            }
        }
        Ok(())
    })
}

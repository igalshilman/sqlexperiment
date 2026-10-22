//! Paired raw/compressed SQL traffic (the default codec, zstd) and isolated codec
//! CPU measurements of every codec.
//! All payloads are deterministic synthetic data; no network or replication model.
use sql::Sql;
use std::{
    fmt::Write as _,
    fs::File,
    hint::black_box,
    io::{BufWriter, Write},
    sync::Arc,
    time::Instant,
};
use volume::{
    PageCompression, Result, StorageOptions, Volume,
    kv::KvStore,
    page_codec,
    simulation::{MemoryKv, Stats},
};

/// Flush and commit `db`'s volume, ending the session.
async fn save(mut db: Sql) -> volume::Result<volume::CommitReport> {
    db.flush().await?;
    db.volume().commit().await
}

/// The codec compared with raw pages in SQL traffic: the default.
const COMPRESSED: PageCompression = PageCompression::Zstd;

#[derive(Clone, Copy, Debug)]
enum Payload {
    Repetitive,
    Json,
    Random,
}
#[derive(Clone, Copy)]
struct Case {
    rows: usize,
    bytes: usize,
    payload: Payload,
}
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
fn payload(case: Case, id: usize, version: usize) -> Vec<u8> {
    let mut state = 0x619d_46e7_130c_985b ^ (id as u64 * 65537) ^ ((version as u64) << 48);
    match case.payload {
        Payload::Repetitive => {
            format!("{:0width$}", id + version * 100_000, width = case.bytes).into_bytes()
        }
        Payload::Random => (0..case.bytes)
            .map(|_| (random(&mut state) >> 32) as u8)
            .collect(),
        Payload::Json => {
            let mut s = format!("{{\"id\":{id},\"version\":{version},\"message\":\"");
            let target = case.bytes - 2;
            let words = [
                "task",
                "scheduled",
                "customer",
                "payment",
                "completed",
                "retry",
                "Berlin",
                "worker",
                "pending",
                "result",
            ];
            while s.len() < target {
                let word = words[(random(&mut state) as usize) % words.len()];
                let remaining = target - s.len();
                s.push_str(&word[..word.len().min(remaining)]);
                if s.len() < target {
                    s.push(' ');
                }
            }
            s.push_str("\"}");
            assert_eq!(s.len(), case.bytes);
            s.into_bytes()
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(s, "{byte:02X}").unwrap();
    }
    s
}
/// No working-set hint: when checkpoints happen depends on encoded sizes, so a
/// hint would differ between codecs and blur the comparison.
fn options(page_compression: PageCompression) -> StorageOptions {
    StorageOptions {
        page_compression,
        hint_pages: 0,
        ..StorageOptions::default()
    }
}
async fn baseline(case: Case, codec: PageCompression) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db =
        Sql::open(&Volume::open_with_options(Arc::new(store.clone()), options(codec)).await?)
            .await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, bucket INTEGER, payload BLOB)")
        .await?;
    db.execute("CREATE INDEX by_bucket ON t(bucket)").await?;
    db.execute("BEGIN").await?;
    for start in (1..=case.rows).step_by(100) {
        let mut sql = String::from("INSERT INTO t VALUES ");
        for id in start..=(start + 99).min(case.rows) {
            if id != start {
                sql.push(',');
            }
            write!(sql, "({id},{},X'{}')", id % 10, hex(&payload(case, id, 0))).unwrap();
        }
        db.execute(&sql).await?;
    }
    db.execute("COMMIT").await?;
    save(db).await?;
    // Steady state: a later save checkpoints the bulk load into page keys.
    let all = StorageOptions {
        checkpoint_after: 0,
        ..options(codec)
    };
    let mut db = Sql::open(&Volume::open_with_options(Arc::new(store.clone()), all).await?).await?;
    db.execute("CREATE TABLE other(x)").await?;
    save(db).await?;
    store.crash();
    Ok(store)
}

struct Sample {
    case: Case,

    codec: PageCompression,
    run: usize,
    operation: &'static str,
}
impl Sample {
    fn emit(
        &self,
        stats: &Stats,
        store: &MemoryKv,
        app_read: usize,
        app_write: usize,
        elapsed_ns: u128,
    ) {
        let read_keys: u64 = stats.by_kind.values().map(|t| t.get_key_bytes).sum();
        let write_keys: u64 = stats
            .by_kind
            .values()
            .map(|t| t.put_key_bytes + t.delete_key_bytes)
            .sum();
        let page = stats.kind("page");
        println!(
            "{},{},{:?},{:?},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.case.rows,
            self.case.bytes,
            self.case.payload,
            self.codec,
            self.run,
            self.operation,
            store.page_count(),
            stats.gets,
            stats.puts,
            stats.deletes,
            stats.bytes_read + read_keys,
            stats.bytes_written + write_keys,
            stats.bytes_written,
            page.bytes_written,
            stats.kind("log").bytes_written,
            stats.kind("commit").bytes_written,
            page.gets,
            page.puts,
            store.stored_bytes(),
            store.stored_key_bytes(),
            app_read,
            app_write,
            elapsed_ns,
            page.bytes_read
        );
    }
}

async fn workload(base: &MemoryKv, sample: Sample) -> Result<()> {
    let case = sample.case;
    let store = Arc::new(base.fork());
    let start = Instant::now();
    let mut db =
        Sql::open(&Volume::open_with_options(store.clone(), options(sample.codec)).await?).await?;
    let middle = case.rows / 2;
    let read_ids: Vec<_> = match sample.operation {
        "cold_pk" => vec![middle],
        "range_10" => (middle..middle + 10).collect(),
        "full_scan" => (1..=case.rows).collect(),
        _ => Vec::new(),
    };
    let changed: Vec<_> = match sample.operation {
        "replace_payload" => vec![middle],
        "ten_adjacent" => (middle..middle + 10).collect(),
        "ten_scattered" => (0..10).map(|i| 1 + i * (case.rows - 1) / 9).collect(),
        _ => Vec::new(),
    };
    if !read_ids.is_empty() {
        // Keep payload verification outside the timed / traffic observation.
        let rows = db
            .query(&format!(
                "SELECT id,hex(payload) FROM t WHERE id BETWEEN {} AND {} ORDER BY id",
                read_ids[0],
                read_ids.last().unwrap()
            ))
            .await?;
        let elapsed = start.elapsed().as_nanos();
        sample.emit(
            &store.stats(),
            &store,
            read_ids.len() * case.bytes,
            0,
            elapsed,
        );
        assert_eq!(rows.len(), read_ids.len());
        for (row, id) in rows.iter().zip(&read_ids) {
            assert_eq!(row[0].as_int(), Some(*id as i64));
            assert_eq!(row[1].to_string(), hex(&payload(case, *id, 0)));
        }
    } else {
        if !changed.is_empty() {
            db.execute("BEGIN").await?;
            for id in &changed {
                db.execute(&format!(
                    "UPDATE t SET payload=X'{}' WHERE id={id}",
                    hex(&payload(case, *id, 1))
                ))
                .await?;
            }
            db.execute("COMMIT").await?;
        }
        save(db).await?;
        let elapsed = start.elapsed().as_nanos();
        sample.emit(
            &store.stats(),
            &store,
            0,
            changed.len() * case.bytes,
            elapsed,
        );
        store.crash();
        // Read with the opposite write policy: stored codec tags must decide.
        let other = if sample.codec == PageCompression::Raw {
            COMPRESSED
        } else {
            PageCompression::Raw
        };
        let mut db =
            Sql::open(&Volume::open_with_options(store.clone(), options(other)).await?).await?;
        assert_eq!(
            db.query("PRAGMA integrity_check").await?[0][0].to_string(),
            "ok"
        );
        for id in &changed {
            assert_eq!(
                db.query(&format!("SELECT hex(payload) FROM t WHERE id={id}"))
                    .await?[0][0]
                    .to_string(),
                hex(&payload(case, *id, 1))
            );
        }
        assert_eq!(
            db.query("SELECT count(*) FROM t").await?[0][0].as_int(),
            Some(case.rows as i64)
        );
    }
    Ok(())
}

async fn pages(store: &MemoryKv) -> Result<Vec<Vec<u8>>> {
    let mut pages = Vec::new();
    for key in store.keys().iter().filter(|k| k.starts_with("v15/page/")) {
        pages.push(page_codec::decode(&store.get(key).await?.unwrap())?);
    }
    Ok(pages)
}

fn codec_bench(out: &mut impl Write, case: Case, pages: &[Vec<u8>], run: usize) -> Result<()> {
    // Evenly spaced, deterministic samples of actual SQL pages, at most 64.
    let count = pages.len().min(64);
    let selected: Vec<_> = (0..count)
        .map(|i| &pages[i * pages.len() / count])
        .collect();
    let iterations = 200;
    // Reverse order on alternate runs to reduce consistent ordering bias.
    let mut policies = [
        PageCompression::Raw,
        PageCompression::Lz4,
        PageCompression::Zstd,
    ];
    policies.rotate_left(run % 3);
    for policy in policies {
        let encoded: Vec<_> = selected
            .iter()
            .map(|p| page_codec::encode(p, policy))
            .collect::<std::io::Result<_>>()?;
        for _ in 0..5 {
            for p in &selected {
                black_box(page_codec::encode(black_box(p), policy)?);
            }
        }
        let start = Instant::now();
        for _ in 0..iterations {
            for p in &selected {
                black_box(page_codec::encode(black_box(p), policy)?);
            }
        }
        let encode_ns = start.elapsed().as_nanos();
        for _ in 0..5 {
            for p in &encoded {
                black_box(page_codec::decode(black_box(p))?);
            }
        }
        let start = Instant::now();
        for _ in 0..iterations {
            for p in &encoded {
                black_box(page_codec::decode(black_box(p))?);
            }
        }
        let decode_ns = start.elapsed().as_nanos();
        for (p, encoded) in selected.iter().zip(&encoded) {
            assert_eq!(&page_codec::decode(encoded)?, *p);
        }
        writeln!(
            out,
            "{},{},{:?},{policy:?},{run},{count},{iterations},{},{},{encode_ns},{decode_ns}",
            case.rows,
            case.bytes,
            case.payload,
            encoded.iter().map(Vec::len).sum::<usize>(),
            encoded.iter().filter(|p| p[0] == 1).count()
        )?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().filter(|arg| arg != "--bench").collect();
    let repeats = args.get(1).map_or(Ok(3), |s| s.parse::<usize>())?;
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    let cpu_path = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("benches/results/page-codec.csv");
    let mut cpu = BufWriter::new(File::create(cpu_path)?);
    writeln!(
        cpu,
        "rows,payload_bytes,payload,codec,run,sample_pages,iterations,encoded_bytes,compressed_pages,encode_ns,decode_ns"
    )?;
    println!(
        "rows,payload_bytes,payload,codec,run,operation,db_pages,gets,puts,deletes,read_key_value_bytes,mutation_key_value_bytes,put_value_bytes,page_write_bytes,log_write_bytes,commit_write_bytes,page_gets,page_puts,stored_value_bytes,stored_key_bytes,app_read_bytes,app_write_bytes,elapsed_ns,page_read_bytes"
    );
    futures::executor::block_on(async {
        for (rows, bytes) in [(100, 64), (100, 512), (100, 9000), (10_000, 256)] {
            for kind in [Payload::Repetitive, Payload::Json, Payload::Random] {
                let case = Case {
                    rows,
                    bytes,
                    payload: kind,
                };
                eprintln!("compression: {rows} rows / {bytes} bytes / {kind:?}");
                let raw = baseline(case, PageCompression::Raw).await?;
                let compressed = baseline(case, COMPRESSED).await?;
                let raw_pages = pages(&raw).await?;
                assert_eq!(
                    raw_pages,
                    pages(&compressed).await?,
                    "codec changed logical SQL pages"
                );
                for run in 0..repeats {
                    codec_bench(&mut cpu, case, &raw_pages, run)?;
                    let policies = if run.is_multiple_of(2) {
                        [PageCompression::Raw, COMPRESSED]
                    } else {
                        [COMPRESSED, PageCompression::Raw]
                    };
                    for codec in policies {
                        let base = if codec == PageCompression::Raw {
                            &raw
                        } else {
                            &compressed
                        };
                        for operation in [
                            "cold_pk",
                            "range_10",
                            "full_scan",
                            "replace_payload",
                            "ten_adjacent",
                            "ten_scattered",
                            "read_only_save",
                        ] {
                            workload(
                                base,
                                Sample {
                                    case,
                                    codec,
                                    run,
                                    operation,
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

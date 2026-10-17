//! End-to-end SQLite commits, separate from correctness tests. No artificial
//! read latency: pending-I/O/cancellation semantics are verified in tests/sqlite.
use anyhow::{Context, Result, ensure};
use quickjs_durable::{Engine, Operation, Outcome, Runtime, Status, sqlite::Host};
use serde_json::json;
use std::{sync::Arc, time::Instant};

fn main() -> Result<()> {
    futures_executor::block_on(run())
}

async fn run() -> Result<()> {
    ensure!(
        !cfg!(debug_assertions) && !cfg!(feature = "verify-dirty"),
        "Use an optimized benchmark without verify-dirty"
    );
    let before = Instant::now();
    let runtime = Arc::new(Runtime::new()?);
    let compile_ms = before.elapsed().as_secs_f64() * 1000.0;
    let directory = tempfile::tempdir()?;
    let cases = [
        ("kv-every-1", 1, ""),
        ("kv-every-10", 10, ""),
        (
            "sparse-8m-every-1",
            1,
            "const bytes=new Uint8Array(8*1024*1024);",
        ),
        ("rpc-every-1", 1, ""),
    ];
    let mut rows = Vec::new();
    let mut runs = Vec::new();
    for (name, batch, prefix) in cases {
        let source = if name == "rpc-every-1" {
            "async function main(input,ctx){for(let i=0;i<input.n;i++){const n=await ctx.rpc.call({service:'Echo',handler:'echo'},i+1);await ctx.kv.set('count',n);}return await ctx.kv.get('count');}".to_owned()
        } else {
            format!(
                "{prefix} async function main(input,ctx){{for(let i=0;i<input.n;i++){{const n=(await ctx.kv.get('count'))??0;await ctx.kv.set('count',n+1);{}}}return await ctx.kv.get('count');}}",
                if prefix.is_empty() {
                    ""
                } else {
                    "bytes[(i*8192)%bytes.length]=i;"
                }
            )
        };
        for repeat in 0..4 {
            let path = directory.path().join(format!("{name}-{repeat}.sqlite"));
            let mut host = Host::open(&path)?;
            let mut engine = Engine::new(host.begin("bench", "scope")?, runtime.clone());
            engine.register("v1", &source)?;
            engine.into_storage()?.commit()?;
            let wal_path = path.with_extension("sqlite-wal");
            let mut peak_wal_bytes = 0;
            let mut next: Option<(String, Outcome, u64)> = None;
            let mut commits = 0;
            let mut result_commits = 0;
            let mut result_store_ms = 0.0;
            let total = Instant::now();
            loop {
                let before = Instant::now();
                let mut engine = Engine::new(host.begin("bench", "scope")?, runtime.clone());
                if name != "rpc-every-1" {
                    engine = engine
                        .with_checkpoint_interval(std::num::NonZeroUsize::new(2 * batch).unwrap());
                }
                let view = match next.take() {
                    None => engine.start("bench", "v1", json!({"n":100}), 1000).await?,
                    Some((id, outcome, now)) => engine.complete("bench", &id, outcome, now).await?,
                };
                let stage_ms = before.elapsed().as_secs_f64() * 1000.0;
                let committed = engine.into_storage()?.commit()?;
                let turn_ms = before.elapsed().as_secs_f64() * 1000.0;
                commits += 1;
                peak_wal_bytes = peak_wal_bytes.max(std::fs::metadata(&wal_path)?.len());
                if repeat > 0 {
                    rows.push(
                        json!({"workload":name,"repeat":repeat,"sequence":view.checkpoint_sequence,
                        "turn_ms":turn_ms,"stage_ms":stage_ms,"commit_ms":committed.commit_ms,
                        "image_bytes":view.image_bytes,"changed_pages":view.changed_pages,
                        "examined_pages":view.examined_pages,"blob_bytes":view.new_blob_bytes,
                        "manifest_bytes":view.manifest_bytes,"io":committed.io}),
                    );
                }
                if view.state.status != Status::Waiting {
                    ensure!(
                        view.state.outcome == Some(Outcome::success(100)),
                        "Unexpected terminal result"
                    );
                    break;
                }
                let effect = host
                    .effects("bench")?
                    .into_iter()
                    .next()
                    .context("Missing effect")?;
                let id = effect.operation.id().to_owned();
                let (outcome, now) = match effect.outcome {
                    Some(outcome) => (outcome, effect.completed_at.context("Missing time")?),
                    None => {
                        let Operation::Rpc { input, .. } = effect.operation else {
                            anyhow::bail!("Unexpected effect")
                        };
                        let outcome = Outcome::success(input);
                        let now = 1001 + view.checkpoint_sequence;
                        let before = Instant::now();
                        host.record_result(&id, &outcome, now)?;
                        result_store_ms += before.elapsed().as_secs_f64() * 1000.0;
                        result_commits += 1;
                        (outcome, now)
                    }
                };
                next = Some((id, outcome, now));
            }
            let elapsed_ms = total.elapsed().as_secs_f64() * 1000.0;
            ensure!(
                host.state("scope", "count")? == Some(json!(100)),
                "Wrong committed application state"
            );
            ensure!(host.effects("bench")?.is_empty(), "Unconsumed effects");
            drop(host);
            // Reopen the database, reconstruct a fresh image and verify the result.
            let mut host = Host::open(&path)?;
            let before = Instant::now();
            let recovered =
                Engine::new(host.begin("bench", "scope")?, runtime.clone()).recover("bench")?;
            let recover_ms = before.elapsed().as_secs_f64() * 1000.0;
            ensure!(
                recovered.state.outcome == Some(Outcome::success(100)),
                "Reopen recovery mismatch"
            );
            if repeat > 0 {
                runs.push(json!({"workload":name,"repeat":repeat,"updates":100,"vm_commits":commits,
                    "result_commits":result_commits,"result_store_ms":result_store_ms,
                    "elapsed_ms":elapsed_ms,"reopen_recover_ms":recover_ms,
                    "database_bytes":std::fs::metadata(&path)?.len(),"peak_wal_allocated_bytes":peak_wal_bytes}));
            }
        }
    }
    let rust = std::process::Command::new("rustc")
        .arg("--version")
        .output()?;
    let output = json!({"sqlite":rusqlite::version(),"rustc":String::from_utf8_lossy(&rust.stdout).trim(),
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"build":runtime.build_id(),
        "compile_ms":compile_ms,"journal_mode":"wal","synchronous":"FULL","wal_autocheckpoint":1000,
        "notes":["One warm-up and three measured runs per workload; 100 logical KV updates each.",
        "Host checkpoint interval is 2*batch Ready operations for KV workloads; RPC uses natural waits. No guest checkpoint API.",
        "turn_ms includes BEGIN IMMEDIATE, cold VM restore/execution/capture, SQL staging and COMMIT. No warm VM cache.",
        "commit_ms is SQLite COMMIT only, including any automatic WAL checkpoint; storage is the local container filesystem.",
        "RPC results are durably stored in an extra transaction each; run elapsed_ms includes those commits and effect queries; turn_ms does not.",
        "Compilation, golden registration and final reopen validation are excluded from run elapsed_ms.",
        "checkpoint_write_bytes counts logical checkpoint keys/values, not physical disk writes. Database bytes include golden and all historical snapshots; no GC.",
        "peak_wal_allocated_bytes is file allocation length, not cumulative WAL traffic; SQLite reuses that file.",
        "No network or artificial KV latency in timing runs; yielding reads and cancellation are exercised separately in tests."],
        "rows":rows,"runs":runs});
    std::fs::write("sqlite-bench.json", serde_json::to_vec_pretty(&output)?)?;
    println!(
        "Saved sqlite-bench.json: {} VM commits across {} measured runs",
        rows.len(),
        runs.len()
    );
    Ok(())
}

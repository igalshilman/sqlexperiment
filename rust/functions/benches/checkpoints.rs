//! Turns of a few workloads over a store in memory: `cargo bench --bench
//! checkpoints` (and `--features full-scan`, the control; `-- s3` keeps the
//! checkpoints' packs in an S3 in memory). Each run goes once with its VM
//! live from turn to turn, and once restoring it at every turn, as a new
//! attempt would. Results go to bench-<mode>.json (bench-<mode>-s3.json).
use anyhow::{Result, ensure};
use functions::{Language, Outcome, Program, Programs, Run, Runtime, Scope, Status};
use serde_json::{Value, json};
use state::simulation::{MemoryBlobs, MemoryKv};
use std::{sync::Arc, time::Instant};

fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        println!("Use cargo bench --bench checkpoints for optimized measurements");
        return Ok(());
    }
    ensure!(
        !cfg!(feature = "verify-dirty"),
        "Benchmarks must not include the verification scan"
    );
    let before = Instant::now();
    let runtime = Arc::new(Runtime::new()?);
    let mode = if cfg!(feature = "full-scan") {
        "full-scan"
    } else if runtime.native_tracking() {
        "native"
    } else {
        "barriers"
    };
    let compile_ms = before.elapsed().as_secs_f64() * 1000.0;
    let s3 = std::env::args().any(|arg| arg == "s3");
    let cases = [
        (
            "scalar",
            "export async function main(){let n=0;for(let i=0;i<3;i++){n+=i;await durable.call('observe');}return n;}",
            json!(3),
        ),
        (
            "sparse-256k",
            "const bytes=new Uint8Array(256*1024);export async function main(){for(let i=0;i<3;i++){bytes[i*8192]=i+1;await durable.call('observe');}return bytes[16384];}",
            json!(3),
        ),
        (
            "heap-churn",
            "export async function main(){let a;for(let i=0;i<3;i++){a=Array.from({length:2000},(_,j)=>({id:j,value:'item-'+i+'-'+j}));await durable.call('observe');}return a.length;}",
            json!(2000),
        ),
        (
            "growth-3m",
            "export async function main(){await durable.call('observe');const b=new Uint8Array(3*1024*1024);b[0]=7;b[b.length-1]=9;await durable.call('observe');b[4096]=8;await durable.call('observe');return b[0]+b[4096];}",
            json!(15),
        ),
        (
            "sparse-8m",
            "const bytes=new Uint8Array(8*1024*1024);export async function main(){for(let i=0;i<2;i++){bytes[i*1024*1024]=i+1;await durable.call('observe');}return bytes[0];}",
            json!(1),
        ),
        (
            "write-heavy",
            "const bytes=new Uint8Array(256*1024);export async function main(){for(let t=0;t<3;t++){for(let i=0;i<10000;i++)bytes[(i*97)%bytes.length]^=i;await durable.call('observe');}return bytes[97];}",
            json!(1),
        ),
    ];
    let mut rows = Vec::new();
    for (name, source, expected) in cases {
        // One warm-up, then three repetitions. Same program, run and input:
        // reproducible images.
        for repeat in 0..4 {
            let before = Instant::now();
            let program = Program::compile(&runtime, Language::JavaScript, name, source)?;
            let program_ms = before.elapsed().as_secs_f64() * 1000.0;
            let programs = Arc::new(Programs::new(runtime.clone()).with(program)?);
            for restored in [false, true] {
                let (store, packs) = (MemoryKv::default(), MemoryBlobs::default());
                let mut scope = Scope::new(Arc::new(store.clone()));
                if s3 {
                    scope = scope.checkpoints(Arc::new(packs.clone()));
                }
                let started = Instant::now();
                let mut run = futures_executor::block_on(Run::start(
                    &programs,
                    &scope,
                    "scope",
                    "bench",
                    name,
                    "main",
                    Value::Null,
                    1000,
                ))?;
                let mut turn_ms = started.elapsed().as_secs_f64() * 1000.0;
                let mut written = store.stats().bytes_written as usize;
                let mut writes = (store.stats().puts + store.stats().deletes) as usize;
                let (mut uploaded, mut uploads) = (packs.uploaded(), packs.puts());
                loop {
                    let view = run.view();
                    if repeat != 0 {
                        rows.push(json!({"workload":name,"repeat":repeat,"restored":restored,
                            "turn":view.turns,"turn_ms":turn_ms,"program_ms":program_ms,
                            "memory_bytes":view.memory_bytes,"live_pages":view.live_pages,
                            "changed_pages":view.changed_pages,"examined_pages":view.examined_pages,
                            "entry_bytes":view.entry_bytes,"head_bytes":view.head_bytes,
                            "written_bytes":written,"writes":writes,
                            "s3_bytes":uploaded,"s3_puts":uploads}));
                    }
                    if view.state.status != Status::Waiting {
                        ensure!(
                            view.state.outcome == Some(Outcome::success(expected.clone())),
                            "Wrong result: {:?}",
                            view.state
                        );
                        break;
                    }
                    let operation = view.state.pending[0].id().to_owned();
                    let (bytes, count) = (
                        store.stats().bytes_written as usize,
                        (store.stats().puts + store.stats().deletes) as usize,
                    );
                    let (s3_bytes, s3_puts) = (packs.uploaded(), packs.puts());
                    let started = Instant::now();
                    if restored {
                        run = futures_executor::block_on(Run::start(
                            &programs,
                            &scope,
                            "scope",
                            "bench",
                            name,
                            "main",
                            Value::Null,
                            1000,
                        ))?;
                    }
                    futures_executor::block_on(run.deliver(
                        &operation,
                        Outcome::success(Value::Null),
                        1001 + view.turns,
                    ))?;
                    turn_ms = started.elapsed().as_secs_f64() * 1000.0;
                    written = store.stats().bytes_written as usize - bytes;
                    writes = (store.stats().puts + store.stats().deletes) as usize - count;
                    (uploaded, uploads) = (packs.uploaded() - s3_bytes, packs.puts() - s3_puts);
                }
            }
        }
    }
    let rust = std::process::Command::new("rustc")
        .arg("--version")
        .output()?;
    let output = json!({"rustc":String::from_utf8_lossy(&rust.stdout).trim(),"arch":std::env::consts::ARCH,
        "os":std::env::consts::OS,"wasmtime":"49.0.3",
        "compile_ms":compile_ms,"capture_mode":mode,"packs":if s3 {"s3"} else {"store"},
        "notes":["Optimized Rust; one warm-up and three repetitions; a store in memory, not a network or a disk.",
        "A live turn delivers to the VM kept from the last one: execution, capture, compression and the writes. A restored turn starts a new attempt first: reading the commit and its pages, and a new Wasm instance.",
        "written_bytes: the turn's pages, its commit, and the writes and deletes that follow it, to the store (Restate).",
        "s3_bytes, s3_puts: what the turn put in S3, with -- s3: its checkpoint's pack.",
        "Native mode examines the pages the kernel saw written (userfaultfd write protection, PAGEMAP_SCAN); barriers mode, those marked by Wasm write barriers and host writes. Full-scan mode examines all pages."],"rows":rows});
    let output = serde_json::to_string_pretty(&output)?;
    let file = if s3 {
        format!("bench-{mode}-s3.json")
    } else {
        format!("bench-{mode}.json")
    };
    std::fs::write(file, &output)?;
    println!("{output}");
    Ok(())
}

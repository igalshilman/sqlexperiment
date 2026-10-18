use anyhow::{Result, ensure};
use quickjs_durable::{
    Engine, Kv, MemoryTransaction, Operation, Outcome, Program, Programs, Resolution, Runtime,
    Status, Transaction,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Instant};

#[derive(Default)]
struct MeasuredKv {
    inner: MemoryTransaction,
    bytes: usize,
    writes: usize,
}
impl Kv for MeasuredKv {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(key).await
    }
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.bytes += key.len() + value.len();
        self.writes += 1;
        self.inner.put(key, value).await
    }
    async fn delete(&mut self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
}
impl Transaction for MeasuredKv {
    async fn execute(&mut self, operation: &Operation) -> Result<Resolution> {
        self.inner.execute(operation).await
    }
    async fn acknowledge(&mut self, operation_id: &str, outcome: &Outcome, now: u64) -> Result<()> {
        self.inner.acknowledge(operation_id, outcome, now).await
    }
    async fn abandon(&mut self, run: &str) -> Result<()> {
        self.inner.abandon(run).await
    }
}
fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        println!("Use cargo bench --bench checkpoints for optimized measurements");
        return Ok(());
    }
    ensure!(
        !cfg!(feature = "verify-dirty"),
        "Benchmarks must not include the verification scan"
    );
    let mode = if cfg!(feature = "full-scan") {
        "full-scan"
    } else {
        "tracked"
    };
    let before = Instant::now();
    let runtime = Arc::new(Runtime::new()?);
    let compile_ms = before.elapsed().as_secs_f64() * 1000.0;
    let cases = [
        (
            "scalar",
            "async function main(){let n=0;for(let i=0;i<3;i++){n+=i;await durable.call('observe');}return n;}",
            json!(3),
        ),
        (
            "sparse-256k",
            "const bytes=new Uint8Array(256*1024);async function main(){for(let i=0;i<3;i++){bytes[i*8192]=i+1;await durable.call('observe');}return bytes[16384];}",
            json!(3),
        ),
        (
            "heap-churn",
            "async function main(){let a;for(let i=0;i<3;i++){a=Array.from({length:2000},(_,j)=>({id:j,value:'item-'+i+'-'+j}));await durable.call('observe');}return a.length;}",
            json!(2000),
        ),
        (
            "growth-3m",
            "async function main(){await durable.call('observe');const b=new Uint8Array(3*1024*1024);b[0]=7;b[b.length-1]=9;await durable.call('observe');b[4096]=8;await durable.call('observe');return b[0]+b[4096];}",
            json!(15),
        ),
        (
            "sparse-8m",
            "const bytes=new Uint8Array(8*1024*1024);async function main(){for(let i=0;i<2;i++){bytes[i*1024*1024]=i+1;await durable.call('observe');}return bytes[0];}",
            json!(1),
        ),
        (
            "write-heavy",
            "const bytes=new Uint8Array(256*1024);async function main(){for(let t=0;t<3;t++){for(let i=0;i<10000;i++)bytes[(i*97)%bytes.length]^=i;await durable.call('observe');}return bytes[97];}",
            json!(1),
        ),
    ];
    let mut rows = Vec::new();
    for (name, source, expected) in cases {
        // One warm-up, then three independent runs. Same program/ID/input for
        // reproducible images; MemoryTransaction keeps storage latency out of the result.
        for repeat in 0..4 {
            let before = Instant::now();
            let program = Program::compile(&runtime, name, source)?;
            let program_ms = before.elapsed().as_secs_f64() * 1000.0;
            let programs = Arc::new(Programs::new(runtime.clone()).with(program)?);
            let mut engine = Engine::new(MeasuredKv::default(), programs);
            let mut previous_bytes = 0;
            let mut previous_writes = 0;
            let mut before = Instant::now();
            let mut view =
                futures_executor::block_on(engine.start("bench", name, Value::Null, 1000))?;
            loop {
                let turn_ms = before.elapsed().as_secs_f64() * 1000.0;
                let written_bytes = engine.storage().bytes - previous_bytes;
                let writes = engine.storage().writes - previous_writes;
                previous_bytes = engine.storage().bytes;
                previous_writes = engine.storage().writes;
                let restore_start = Instant::now();
                let restored = futures_executor::block_on(engine.recover("bench"))?;
                let restore_ms = restore_start.elapsed().as_secs_f64() * 1000.0;
                ensure!(
                    view.checkpoint_id == restored.checkpoint_id && view.state == restored.state,
                    "Recovery mismatch"
                );
                if repeat != 0 {
                    rows.push(
                        json!({"workload":name,"repeat":repeat,"turn":view.checkpoint_sequence,
                        "turn_ms":turn_ms,"restore_ms":restore_ms,"program_ms":program_ms,
                        "image_bytes":view.image_bytes,
                        "changed_pages":view.changed_pages,"override_pages":view.override_pages,
                        "examined_pages":view.examined_pages,"captured_bytes":view.changed_pages*4096,
                        "new_blob_bytes":view.new_blob_bytes,"manifest_bytes":view.manifest_bytes,
                        "written_bytes":written_bytes,"writes":writes}),
                    );
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
                before = Instant::now();
                view = futures_executor::block_on(engine.complete(
                    "bench",
                    &operation,
                    Outcome::success(Value::Null),
                    1001 + view.checkpoint_sequence,
                ))?;
            }
        }
    }
    let rust = std::process::Command::new("rustc")
        .arg("--version")
        .output()?;
    let output = json!({"rustc":String::from_utf8_lossy(&rust.stdout).trim(),"arch":std::env::consts::ARCH,
        "os":std::env::consts::OS,"wasmtime":"49.0.3",
        "compile_ms":compile_ms,"capture_mode":mode,"native_tracking_probe":quickjs_durable::tracking::probe(),
        "notes":["Optimized Rust; one warm-up and three repetitions; MemoryTransaction, not fsync or network storage.",
        "Turn includes cold load, fresh Wasmtime instance, guest execution, capture, compression and staged checkpoint KV writes; host transaction commit is excluded. Reliable KV; no integrity hashes.",
        "Restore measures engine.recover with no event: decode/reconstruct committed image and state; does not instantiate or enter Wasm.",
        "written_bytes includes staged checkpoint keys/values, initial input metadata, blob, manifest and head; no operation log. Host effect encoding and outer commit are excluded.",
        "Tracked mode examines only pages marked by Wasm write barriers and host writes. Full-scan mode compiles the original uninstrumented module and examines all pages.",
        "Both modes copy only changed pages during capture; cold restoration still touches full memory.",
        "No native dirty tracking or COW golden mapping; examined_pages includes written-then-restored pages in tracked mode."],"rows":rows});
    let output = serde_json::to_string_pretty(&output)?;
    std::fs::write(format!("bench-{mode}.json"), &output)?;
    println!("{output}");
    Ok(())
}

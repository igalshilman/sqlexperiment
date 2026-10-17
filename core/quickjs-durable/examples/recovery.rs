use anyhow::{Context, Result, ensure};
use quickjs_durable::{Engine, FileKv, MemoryTransaction, Outcome, Runtime};
use serde_json::Value;
use std::{path::PathBuf, process::Command, sync::Arc};

const SOURCE: &str = "async function main(){const a=await durable.call('number',{value:20});const b=await durable.call('number',{value:22});return a+b;}";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 {
        let dir =
            std::env::temp_dir().join(format!("quickjs-tx-process-demo-{}", std::process::id()));
        for action in ["start", "recover", "first", "recover", "second", "recover"] {
            ensure!(
                Command::new(&args[0])
                    .args([action, dir.to_str().context("Invalid directory")?])
                    .status()?
                    .success(),
                "Child failed"
            );
        }
        println!("Persisted host transaction domain: {}", dir.display());
        return Ok(());
    }
    let dir = PathBuf::from(args.get(2).context("Expected directory")?);
    let mut store = FileKv::open(dir)?;
    let tx: MemoryTransaction = store
        .get("domain")?
        .map(|b| serde_json::from_slice(&b))
        .transpose()?
        .unwrap_or_default();
    let mut engine = Engine::new(tx, Arc::new(Runtime::new()?));
    let view = match args[1].as_str() {
        "start" => {
            futures_executor::block_on(engine.register("demo-v1", SOURCE))?;
            futures_executor::block_on(engine.start("demo", "demo-v1", Value::Null, 1000))?
        }
        "first" => futures_executor::block_on(engine.complete(
            "demo",
            "demo/0",
            Outcome::success(20),
            1001,
        ))?,
        "second" => futures_executor::block_on(engine.complete(
            "demo",
            "demo/1",
            Outcome::success(22),
            1002,
        ))?,
        "recover" => futures_executor::block_on(engine.recover("demo"))?,
        _ => anyhow::bail!("Unknown action"),
    };
    if args[1] != "recover" {
        store.put("domain", &serde_json::to_vec(&engine.into_storage()?)?)?;
    }
    println!(
        "process={} action={} {}",
        std::process::id(),
        args[1],
        serde_json::to_string(&view)?
    );
    Ok(())
}

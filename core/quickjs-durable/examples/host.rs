//! Host-owned atomic commit and explicit recovery. No replay log.
//!
//! cargo run --release --example host -- [directory]
use anyhow::{Context, Result, bail};
use quickjs_durable::{
    Engine, FileKv, MemoryTransaction, Operation, Outcome, Programs, Runtime, Status,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const SOURCE: &str = r#"async function main(input,ctx){
    const old=(await ctx.kv.get('total'))??0;
    const amount=await ctx.rpc.call({service:'Calculator',handler:'double'},input);
    await ctx.kv.set('total',old+amount);
    await ctx.kv.set('temporary',true);await ctx.kv.delete('temporary');
    
    return {total:await ctx.kv.get('total'),temporary:await ctx.kv.get('temporary')};
}"#;

fn main() -> Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("quickjs-tx-demo-{}", std::process::id()))
        });
    let mut store = FileKv::open(&directory)?;
    // The programs this host serves, each compiled once into its golden image.
    let programs =
        Arc::new(Programs::new(Arc::new(Runtime::new()?)).compile("transaction-demo-v1", SOURCE)?);
    // The demo host atomically publishes its whole domain as one KV value. A
    // real host supplies its own multi-key transaction spanning the same data.
    let initial: MemoryTransaction = store
        .get("domain")?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?
        .unwrap_or_default();
    let mut engine = Engine::new(initial, programs.clone());
    let mut view =
        futures_executor::block_on(engine.start("r", "transaction-demo-v1", json!(21), 1000))?;
    store.put("domain", &serde_json::to_vec(&engine.into_storage()?)?)?; // COMMIT
    while view.state.status == Status::Waiting {
        // Explicit recovery: restore committed state, locate the pending effect,
        // and deliver its completion through the host's own path.
        let committed: MemoryTransaction =
            serde_json::from_slice(&store.get("domain")?.context("Missing domain")?)?;
        let recovered = futures_executor::block_on(
            Engine::new(committed.clone(), programs.clone()).recover("r"),
        )?;
        let id = recovered.state.pending[0].id().to_owned();
        let effect = committed.effects.get(&id).context("Missing host effect")?;
        let outcome = match effect {
            Operation::Rpc { target, input, .. }
                if target.service == "Calculator" && target.handler == "double" =>
            {
                Outcome::success(input.as_i64().context("Expected integer")? * 2)
            }
            Operation::Resume { .. } => Outcome::success(Value::Null),
            _ => bail!("Bind a host transport/scheduler for this effect"),
        };
        // The local Calculator is pure Rust. Remote effects would be completed
        // by the host transport, which supplies its durable result here.
        let mut engine = Engine::new(committed, programs.clone())
            .with_checkpoint_interval(std::num::NonZeroUsize::new(3).unwrap());
        view = futures_executor::block_on(engine.complete(
            "r",
            &id,
            outcome,
            1001 + recovered.checkpoint_sequence,
        ))?;
        // Consumption, local KV writes, new effects, and VM state commit together.
        store.put("domain", &serde_json::to_vec(&engine.into_storage()?)?)?;
    }
    println!("{}", serde_json::to_string_pretty(&view.state.outcome)?);
    println!(
        "Run again: cargo run --release --example host -- {}",
        directory.display()
    );
    Ok(())
}

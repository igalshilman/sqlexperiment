//! cargo run --release --features sqlite-host --example sqlite -- /tmp/qjd.sqlite
//! Run again against the same file to recover the terminal result without adding
//! to the counter. The Calculator is pure Rust; a remote transport would use the
//! same durable effect IDs and deliver results through record_result.
use anyhow::{Context, Result, bail};
use quickjs_durable::{Engine, Operation, Outcome, Programs, Runtime, Status, sqlite::Host};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Instant};

const SOURCE: &str = r#"async function main(input,ctx){
  for(let i=0;i<input.steps;i++){
    const n=(await ctx.kv.get('count'))??0;
    await ctx.kv.set('count',n+1);
  }
  const count=await ctx.kv.get('count');
  const answer=await ctx.rpc.call({service:'Calculator',handler:'double'},count);
  await ctx.kv.set('answer',answer);
  return {count,answer};
}"#;

fn main() -> Result<()> {
    futures_executor::block_on(run())
}

async fn run() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("quickjs-checkpoints.sqlite"));
    let mut host = Host::open(&path)?;
    // Compiled once, outside the measured VM turns: every run starts from its
    // golden image.
    let programs =
        Arc::new(Programs::new(Arc::new(Runtime::new()?)).compile("sqlite-demo-v1", SOURCE)?);
    println!("SQLite {}: WAL, synchronous=FULL", rusqlite::version());
    println!("sequence | changed pages | blob bytes | VM/stage ms | COMMIT ms");
    let mut next: Option<(String, Outcome, u64)> = None;
    loop {
        let before = Instant::now();
        let mut engine = Engine::new(host.begin("example", "counter")?, programs.clone())
            .with_checkpoint_interval(std::num::NonZeroUsize::new(2).unwrap());
        let view = match next.take() {
            None => {
                engine
                    .start("example", "sqlite-demo-v1", json!({"steps":10}), 1000)
                    .await?
            }
            Some((id, outcome, now)) => engine.complete("example", &id, outcome, now).await?,
        };
        let stage_ms = before.elapsed().as_secs_f64() * 1000.0;
        let committed = engine.into_storage()?.commit()?;
        println!(
            "{:8} | {:13} | {:10} | {:11.3} | {:9.3}",
            view.checkpoint_sequence,
            view.changed_pages,
            view.new_blob_bytes,
            stage_ms,
            committed.commit_ms
        );
        if view.state.status != Status::Waiting {
            println!("{}", serde_json::to_string_pretty(&view.state.outcome)?);
            println!(
                "Committed KV: count={:?}, answer={:?}",
                host.state("counter", "count")?,
                host.state("counter", "answer")?
            );
            println!("Database: {}", path.display());
            return Ok(());
        }
        // Dispatch only after COMMIT. Checkpoint wakeups are already ready.
        let effect = host
            .effects("example")?
            .into_iter()
            .next()
            .context("Missing effect")?;
        if effect.outcome.is_none() {
            let outcome = match &effect.operation {
                Operation::Rpc { target, input, .. }
                    if target.service == "Calculator" && target.handler == "double" =>
                {
                    Outcome::success(input.as_i64().context("Expected integer")? * 2)
                }
                _ => bail!("Bind this effect to a host transport or timer"),
            };
            host.record_result(
                effect.operation.id(),
                &outcome,
                1001 + view.checkpoint_sequence,
            )?;
        }
        let ready = host
            .effects("example")?
            .into_iter()
            .next()
            .context("Missing result")?;
        next = Some((
            ready.operation.id().to_owned(),
            ready.outcome.context("Result not ready")?,
            ready.completed_at.context("Missing completion time")?,
        ));
    }
}

#![cfg(unix)]
use anyhow::{Result, ensure};
use quickjs_durable::{Engine, FileKv, Kv, MemoryTransaction, Outcome, Runtime, Status};
use serde_json::Value;
use std::{process::Command, sync::Arc};

const SOURCE: &str = "async function main(_,ctx){await ctx.kv.set('n',0);const n=await ctx.rpc.call({service:'Counter',handler:'next'});await ctx.kv.set('n',n);return n;}";
fn kill() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort()
}
#[test]
fn child_crash() -> Result<()> {
    let Ok(dir) = std::env::var("QJD_CRASH_DIRECTORY") else {
        return Ok(());
    };
    let after = std::env::var("QJD_CRASH_AFTER")? == "true";
    let mut store = FileKv::open(dir)?;
    let tx: MemoryTransaction = serde_json::from_slice(&store.get("domain")?.unwrap())?;
    let mut engine = Engine::new(tx, Arc::new(Runtime::new()?))
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    let pending = engine.recover("run")?.state.pending[0].id().to_owned();
    futures_executor::block_on(engine.complete("run", &pending, Outcome::success(42), 1001))?;
    let bytes = serde_json::to_vec(&engine.into_storage()?)?;
    if !after {
        kill();
    }
    store.put("domain", &bytes)?; // the host's sole atomic commit
    kill();
}
#[test]
fn sigkill_before_and_after_host_commit_preserves_atomic_domain() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let runtime = Arc::new(Runtime::new()?);
    for after in [false, true] {
        let dir = tempfile::tempdir()?;
        let mut store = FileKv::open(dir.path())?;
        let mut engine = Engine::new(MemoryTransaction::default(), runtime.clone());
        engine.register("v1", SOURCE)?;
        let start = futures_executor::block_on(engine.start("run", "v1", Value::Null, 1000))?;
        store.put("domain", &serde_json::to_vec(&engine.into_storage()?)?)?;
        let status = Command::new(std::env::current_exe()?)
            .args(["--exact", "child_crash", "--nocapture"])
            .env("QJD_CRASH_DIRECTORY", dir.path())
            .env("QJD_CRASH_AFTER", after.to_string())
            .status()?;
        ensure!(
            status.signal() == Some(libc::SIGKILL),
            "Unexpected exit: {status}"
        );
        let tx: MemoryTransaction = serde_json::from_slice(&store.get("domain")?.unwrap())?;
        assert_eq!(tx.state["n"], if after { 42 } else { 0 });
        assert_eq!(tx.effects.len(), 1);
        let mut engine = Engine::new(tx, runtime.clone())
            .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
        let recovered = engine.recover("run")?;
        assert_eq!(recovered.checkpoint_sequence, if after { 1 } else { 0 });
        assert!(
            engine
                .storage()
                .effects
                .contains_key(recovered.state.pending[0].id())
        );
        // Redelivery is chosen by the host; recover itself executed no code.
        let next = futures_executor::block_on(engine.complete(
            "run",
            start.state.pending[0].id(),
            Outcome::success(42),
            1002,
        ))?;
        assert_eq!(next.checkpoint_sequence, 1);
        let done = futures_executor::block_on(engine.complete(
            "run",
            next.state.pending[0].id(),
            Outcome::success(Value::Null),
            1003,
        ))?;
        assert_eq!(done.state.status, Status::Completed);
        assert_eq!(done.state.outcome, Some(Outcome::success(42)));
        assert!(engine.storage().effects.is_empty());
    }
    Ok(())
}

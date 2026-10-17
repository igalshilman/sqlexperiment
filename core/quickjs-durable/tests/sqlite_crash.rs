#![cfg(all(unix, feature = "sqlite-host"))]
use anyhow::{Context, Result, ensure};
use futures_executor::block_on;
use quickjs_durable::{Engine, Outcome, Runtime, Status, sqlite::Host};
use serde_json::{Value, json};
use std::{os::unix::process::ExitStatusExt, process::Command, sync::Arc};
const SOURCE: &str = "async function main(_,ctx){await ctx.kv.set('n',0);const n=await ctx.rpc.call({service:'Counter',handler:'next'});await ctx.kv.set('n',n);return n;}";
fn kill() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort()
}
#[test]
fn sqlite_crash_child() -> Result<()> {
    let Ok(path) = std::env::var("QJD_SQLITE_CRASH_PATH") else {
        return Ok(());
    };
    let completion = std::env::var("QJD_SQLITE_COMPLETION")? == "true";
    let after = std::env::var("QJD_SQLITE_AFTER")? == "true";
    let mut host = Host::open(path)?;
    let runtime = Arc::new(Runtime::new()?);
    let mut engine = Engine::new(host.begin("r", "scope")?, runtime);
    if completion {
        engine = engine.with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
        let id = futures_executor::block_on(engine.recover("r"))?
            .state
            .pending[0]
            .id()
            .to_owned();
        block_on(engine.complete("r", &id, Outcome::success(42), 1001))?;
    } else {
        block_on(engine.start("r", "v1", Value::Null, 1000))?;
    }
    if !after {
        kill();
    }
    engine.into_storage()?.commit()?;
    kill();
}
#[test]
fn sigkill_around_start_and_completion_commits_recovers_whole_sqlite_domain() -> Result<()> {
    let runtime = Arc::new(Runtime::new()?);
    for completion in [false, true] {
        for after in [false, true] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("host.sqlite");
            let mut host = Host::open(&path)?;
            let mut engine = Engine::new(host.begin("r", "scope")?, runtime.clone());
            futures_executor::block_on(engine.register("v1", SOURCE))?;
            engine.into_storage()?.commit()?;
            if completion {
                let mut engine = Engine::new(host.begin("r", "scope")?, runtime.clone());
                let start = block_on(engine.start("r", "v1", Value::Null, 1000))?;
                engine.into_storage()?.commit()?;
                host.record_result(start.state.pending[0].id(), &Outcome::success(42), 1001)?;
            }
            drop(host);
            let status = Command::new(std::env::current_exe()?)
                .args(["--exact", "sqlite_crash_child", "--nocapture"])
                .env("QJD_SQLITE_CRASH_PATH", &path)
                .env("QJD_SQLITE_COMPLETION", completion.to_string())
                .env("QJD_SQLITE_AFTER", after.to_string())
                .status()?;
            ensure!(
                status.signal() == Some(libc::SIGKILL),
                "Unexpected child exit: {status}"
            );
            let mut host = Host::open(&path)?;
            let expected = if completion && after {
                Some(json!(42))
            } else if completion || after {
                Some(json!(0))
            } else {
                None
            };
            assert_eq!(host.state("scope", "n")?, expected);
            let effects = host.effects("r")?;
            assert_eq!(effects.len(), usize::from(completion || after));
            if completion && !after {
                assert_eq!(effects[0].outcome, Some(Outcome::success(42)));
            }
            let mut engine = Engine::new(host.begin("r", "scope")?, runtime.clone());
            if !completion && !after {
                assert!(futures_executor::block_on(engine.recover("r")).is_err());
            } else {
                assert_eq!(
                    futures_executor::block_on(engine.recover("r"))?.checkpoint_sequence,
                    u64::from(completion && after)
                );
            }
            // The host redelivers start if necessary. Committed starts are
            // idempotent; available completions are read from current effect rows.
            let mut view = block_on(engine.start("r", "v1", Value::Null, 1000))?;
            engine.into_storage()?.commit()?;
            while view.state.status == Status::Waiting {
                let effect = host
                    .effects("r")?
                    .into_iter()
                    .next()
                    .context("Missing effect")?;
                if effect.outcome.is_none() {
                    host.record_result(effect.operation.id(), &Outcome::success(42), 1001)?;
                }
                let ready = host.effects("r")?.into_iter().next().unwrap();
                let mut engine = Engine::new(host.begin("r", "scope")?, runtime.clone());
                view = block_on(engine.complete(
                    "r",
                    ready.operation.id(),
                    ready.outcome.unwrap(),
                    ready.completed_at.unwrap(),
                ))?;
                engine.into_storage()?.commit()?;
            }
            assert_eq!(view.state.outcome, Some(Outcome::success(42)));
            assert_eq!(host.state("scope", "n")?, Some(json!(42)));
            assert!(host.effects("r")?.is_empty());
        }
    }
    Ok(())
}

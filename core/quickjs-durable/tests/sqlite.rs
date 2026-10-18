#![cfg(feature = "sqlite-host")]
use anyhow::Result;
use common::programs;
use futures_channel::oneshot;
use futures_executor::block_on;
use quickjs_durable::{
    Engine, Kv, Operation, Outcome, Resolution, Status, Transaction,
    sqlite::{Host, SqliteTransaction},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
};

mod common;

fn count(reader: &Connection, sql: &str) -> i64 {
    reader.query_row(sql, [], |r| r.get(0)).unwrap()
}

// Controllable genuinely-pending I/O, used only in tests. No sleep, threads or
// simulated remote-read mode in the SQLite host itself.
struct GatedRead<'a> {
    inner: SqliteTransaction<'a>,
    gate: Option<oneshot::Receiver<()>>,
}
impl Kv for GatedRead<'_> {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(key).await
    }
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.inner.put(key, value).await
    }
}
impl Transaction for GatedRead<'_> {
    fn check_run(&self, run: &str) -> Result<()> {
        self.inner.check_run(run)
    }
    async fn execute(&mut self, op: &Operation) -> Result<Resolution> {
        if matches!(op, Operation::KvGet { .. })
            && let Some(gate) = self.gate.take()
        {
            gate.await?;
        }
        self.inner.execute(op).await
    }
    async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<()> {
        self.inner.acknowledge(id, outcome, now).await
    }
    async fn abandon(&mut self, run: &str) -> Result<()> {
        self.inner.abandon(run).await
    }
}

#[test]
fn pending_kv_read_retains_transaction_and_produces_only_one_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("host.sqlite");
    let mut host = Host::open(&path)?;
    let programs = programs(
        "async function main(_,ctx){await ctx.kv.set('n',1);const n=await ctx.kv.get('n');await ctx.kv.set('n',n+1);return n+1;}",
    );
    let reader = Connection::open(&path)?;
    let (send, receive) = oneshot::channel();
    let tx = GatedRead {
        inner: host.begin("r", "scope")?,
        gate: Some(receive),
    };
    let mut engine = Engine::new(tx, programs.clone());
    let mut future = Box::pin(engine.start("r", "v1", Value::Null, 1000));
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(count(&reader, "SELECT count(*) FROM state"), 0);
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM checkpoints WHERE key LIKE 'checkpoint/%'"
        ),
        0
    );
    assert_eq!(count(&reader, "SELECT count(*) FROM effects"), 0);
    send.send(()).unwrap();
    let view = block_on(future)?;
    assert_eq!(view.checkpoint_sequence, 0);
    assert_eq!(view.state.outcome, Some(Outcome::success(2)));
    assert_eq!(count(&reader, "SELECT count(*) FROM state"), 0); // still provisional
    let stats = engine.into_storage()?.inner.commit()?;
    assert_eq!(stats.io.kv_reads, 1);
    assert_eq!(stats.io.kv_writes, 2);
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM checkpoints WHERE key LIKE 'checkpoint/%'"
        ),
        1
    );
    assert_eq!(host.state("scope", "n")?, Some(json!(2)));
    Ok(())
}

#[test]
fn cancelling_a_pending_read_rolls_back_state_effects_and_input() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("host.sqlite");
    let mut host = Host::open(&path)?;
    let programs = programs(
        "async function main(_,ctx){await ctx.kv.set('n',1);const p=ctx.rpc.call({service:'Echo',handler:'echo'},1);await ctx.kv.get('n');return await p;}",
    );
    let (_send, receive) = oneshot::channel();
    let mut engine = Engine::new(
        GatedRead {
            inner: host.begin("r", "scope")?,
            gate: Some(receive),
        },
        programs.clone(),
    );
    let mut future = Box::pin(engine.start("r", "v1", Value::Null, 1000));
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(future);
    assert_eq!(engine.storage().inner.io().effect_inserts, 1);
    assert_eq!(engine.storage().inner.io().kv_writes, 1);
    // Even if the caller mistakenly tries to commit after cancellation, the
    // engine must refuse extraction and drop the SQLite transaction.
    assert!(futures_executor::block_on(engine.recover("r")).is_err());
    assert!(engine.into_storage().is_err());
    drop(host);
    let mut host = Host::open(&path)?;
    assert!(host.state("scope", "n")?.is_none());
    assert!(host.effects("r")?.is_empty());
    let reader = Connection::open(&path)?;
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM checkpoints WHERE key LIKE 'run/%' OR key LIKE 'checkpoint/%'"
        ),
        0
    );
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    assert!(futures_executor::block_on(engine.recover("r")).is_err());
    let retry = block_on(engine.start("r", "v1", Value::Null, 1001))?;
    assert_eq!(retry.state.status, Status::Waiting);
    engine.into_storage()?.commit()?;
    assert_eq!(host.effects("r")?.len(), 1);
    Ok(())
}

const FLOW: &str = "async function main(_,ctx){await ctx.kv.set('n',0);const n=await ctx.rpc.call({service:'Counter',handler:'next'});await ctx.kv.set('n',n);return n;}";

#[test]
fn completion_consumption_rolls_back_with_vm_and_state_then_commits_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("host.sqlite");
    let mut host = Host::open(&path)?;
    let programs = programs(FLOW);
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    let first = block_on(engine.start("r", "v1", Value::Null, 1000))?;
    engine.into_storage()?.commit()?;
    let id = first.state.pending[0].id();
    assert!(host.record_result(id, &Outcome::success(42), 1001)?);
    assert!(!host.record_result(id, &Outcome::success(999), 1002)?);
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone())
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    let provisional = block_on(engine.complete("r", id, Outcome::success(42), 1001))?;
    assert_eq!(provisional.checkpoint_sequence, 1);
    drop(engine); // abort consumption, KV update, wakeup and image together
    drop(host);
    let mut host = Host::open(&path)?;
    assert_eq!(host.state("scope", "n")?, Some(json!(0)));
    let effects = host.effects("r")?;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].outcome, Some(Outcome::success(42)));
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone())
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    assert_eq!(
        futures_executor::block_on(engine.recover("r"))?.checkpoint_id,
        first.checkpoint_id
    );
    assert_eq!(engine.storage().io().checkpoint_writes, 0);
    let next = block_on(engine.complete("r", id, Outcome::success(42), 1001))?;
    let stats = engine.into_storage()?.commit()?;
    assert_eq!(stats.io.effect_consumptions, 1);
    assert_eq!(stats.io.effect_inserts, 1);
    assert_eq!(host.state("scope", "n")?, Some(json!(42)));
    let effects = host.effects("r")?;
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0].operation, Operation::Resume { .. }));
    assert_eq!(effects[0].outcome, Some(Outcome::success(Value::Null)));
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone())
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    let duplicate = block_on(engine.complete("r", id, Outcome::success(999), 1002))?;
    assert_eq!(duplicate.checkpoint_id, next.checkpoint_id);
    assert_eq!(engine.storage().io().effect_consumptions, 0);
    let done = block_on(engine.complete(
        "r",
        effects[0].operation.id(),
        effects[0].outcome.clone().unwrap(),
        1002,
    ))?;
    assert_eq!(done.state.outcome, Some(Outcome::success(42)));
    engine.into_storage()?.commit()?;
    assert!(host.effects("r")?.is_empty());
    assert!(!host.record_result(id, &Outcome::success(42), 1003)?);
    Ok(())
}

#[test]
fn a_trap_ends_the_run_in_one_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("host.sqlite");
    let mut host = Host::open(&path)?;
    let programs = programs(
        "async function main(_,ctx){await ctx.kv.set('n',1);ctx.rpc.call({service:'Echo',handler:'echo'},1);await ctx.kv.get('n');while(true){} }",
    );
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    let failed = block_on(engine.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(failed.state.status, quickjs_durable::Status::Failed);
    assert_eq!(engine.storage().io().effect_inserts, 1);
    engine.into_storage()?.commit()?;
    assert_eq!(host.state("scope", "n")?, Some(Value::from(1)));
    assert!(host.effects("r")?.is_empty());
    let recovered =
        block_on(Engine::new(host.begin("r", "scope")?, programs.clone()).recover("r"))?;
    assert_eq!(recovered.state.status, quickjs_durable::Status::Failed);
    Ok(())
}

#[test]
fn application_scope_is_host_selected_and_reads_observe_prior_commits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut host = Host::open(dir.path().join("host.sqlite"))?;
    let programs = programs(
        "async function main(_,ctx){const n=(await ctx.kv.get('n'))??0;await ctx.kv.set('n',n+1);await ctx.kv.set('temp',1);await ctx.kv.delete('temp');return n+1;}",
    );
    for (run, scope, expected) in [("r1", "A", 1), ("r2", "B", 1), ("r3", "A", 2)] {
        let mut engine = Engine::new(host.begin(run, scope)?, programs.clone());
        assert_eq!(
            block_on(engine.start(run, "v1", Value::Null, 1000))?
                .state
                .outcome,
            Some(Outcome::success(expected))
        );
        engine.into_storage()?.commit()?;
        assert!(host.state(scope, "temp")?.is_none());
    }
    assert_eq!(host.state("A", "n")?, Some(json!(2)));
    assert_eq!(host.state("B", "n")?, Some(json!(1)));
    Ok(())
}

#[test]
fn delivery_must_match_the_durable_result() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut host = Host::open(dir.path().join("host.sqlite"))?;
    let programs = programs(FLOW);
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    let first = block_on(engine.start("r", "v1", Value::Null, 1000))?;
    engine.into_storage()?.commit()?;
    let id = first.state.pending[0].id();
    host.record_result(id, &Outcome::success(42), 1005)?;
    for (outcome, now) in [(Outcome::success(999), 1005), (Outcome::success(42), 1004)] {
        let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
        assert!(block_on(engine.complete("r", id, outcome, now)).is_err());
        drop(engine);
        assert_eq!(host.effects("r")?[0].outcome, Some(Outcome::success(42)));
        assert_eq!(host.state("scope", "n")?, Some(json!(0)));
    }
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    let done = block_on(engine.complete("r", id, Outcome::success(42), 1006))?;
    assert_eq!(done.state.outcome, Some(Outcome::success(42)));
    engine.into_storage()?.commit()?;
    Ok(())
}

#[test]
fn committed_run_scope_cannot_change_on_recovery() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("host.sqlite");
    let mut host = Host::open(&path)?;
    let programs = programs(FLOW);
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    block_on(engine.start("r", "v1", Value::Null, 1000))?;
    engine.into_storage()?.commit()?;
    drop(host);
    let mut host = Host::open(&path)?;
    assert!(host.begin("r", "different-scope").is_err());
    assert!(
        futures_executor::block_on(
            Engine::new(host.begin("r", "scope")?, programs.clone()).recover("r")
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn transaction_checks_run_even_without_guest_requests() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut host = Host::open(dir.path().join("host.sqlite"))?;
    let programs = programs("function main(){return 42;}");
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    block_on(engine.start("r", "v1", Value::Null, 1000))?;
    engine.into_storage()?.commit()?;
    let mut engine = Engine::new(host.begin("other", "scope")?, programs.clone());
    assert!(futures_executor::block_on(engine.recover("r")).is_err());
    assert!(block_on(engine.start("r", "v1", Value::Null, 1000)).is_err());
    assert!(engine.into_storage().is_err());
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    assert!(block_on(engine.start("new-run", "v1", Value::Null, 1000)).is_err());
    assert!(engine.into_storage().is_err());
    Ok(())
}

#[test]
fn unusable_timer_results_do_not_poison_the_inbox() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut host = Host::open(dir.path().join("host.sqlite"))?;
    let programs = programs("async function main(_,ctx){await ctx.sleep(10);return 42;}");
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    let first = block_on(engine.start("r", "v1", Value::Null, 1000))?;
    engine.into_storage()?.commit()?;
    let id = first.state.pending[0].id();
    assert!(
        host.record_result(id, &Outcome::success(Value::Null), u64::MAX / 1_000_000 + 1)
            .is_err()
    );
    assert!(
        host.record_result(id, &Outcome::success(Value::Null), 1009)
            .is_err()
    );
    assert!(host.effects("r")?[0].outcome.is_none());
    assert!(host.record_result(id, &Outcome::success(Value::Null), 1010)?);
    let mut engine = Engine::new(host.begin("r", "scope")?, programs.clone());
    assert_eq!(
        block_on(engine.complete("r", id, Outcome::success(Value::Null), 1010))?
            .state
            .outcome,
        Some(Outcome::success(42))
    );
    engine.into_storage()?.commit()?;
    Ok(())
}

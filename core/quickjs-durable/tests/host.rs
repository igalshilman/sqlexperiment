use anyhow::{Result, bail};
use common::programs;
use quickjs_durable::{
    Engine, Kv, MemoryTransaction, Operation, Outcome, Resolution, Status, Transaction, View,
};
use serde_json::{Value, json};

mod common;

fn value(view: &View) -> Value {
    assert_eq!(view.state.status, Status::Completed);
    match &view.state.outcome {
        Some(Outcome::Success { value, .. }) => value.clone(),
        other => panic!("Unexpected outcome: {other:?}"),
    }
}
fn image(tx: &MemoryTransaction) -> Value {
    serde_json::to_value(tx).unwrap()
}

#[test]
fn local_kv_awaits_share_one_checkpoint_and_observe_staged_writes() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        await Promise.resolve();
        const v={n:1};const set=ctx.kv.set('x',v);v.n=99;
        await set;const seen=await ctx.kv.get('x');
        await ctx.kv.set('temporary',true);await ctx.kv.delete('temporary');
        return {seen,missing:await ctx.kv.get('temporary')};}"#,
    );
    let committed = MemoryTransaction::default();
    let mut engine = Engine::new(committed.clone(), programs.clone());
    let view = futures_executor::block_on(engine.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(value(&view), json!({"seen":{"n":1},"missing":null}));
    assert_eq!(view.checkpoint_sequence, 0);
    assert_eq!(
        engine
            .storage()
            .values
            .keys()
            .filter(|k| k.starts_with("checkpoint/"))
            .count(),
        1
    );
    assert_eq!(engine.storage().state["x"], json!({"n":1}));
    assert!(engine.storage().effects.is_empty());
    assert!(committed.state.is_empty()); // staging did not publish anything
    assert!(committed.values.keys().all(|k| !k.ends_with("/head")));
    let committed = engine.into_storage()?; // host's atomic publish
    let recovered =
        futures_executor::block_on(Engine::new(committed, programs.clone()).recover("r"))?;
    assert_eq!(recovered.checkpoint_id, view.checkpoint_id);
    Ok(())
}

const FLOW: &str = r#"async function main(_,ctx){
    await ctx.kv.set('n',1);
    const n=await ctx.rpc.call({service:'Counter',handler:'next',key:'shared'});
    await ctx.kv.set('n',n);
    await ctx.rpc.call({service:'Audit',handler:'record'},{n});
    return n;
}"#;

// Fault injection is on staged mutations, not unreliable KV bytes. The host
// discards the entire workspace on error. Counts include effect consumption.
struct FaultTransaction {
    inner: MemoryTransaction,
    actions: usize,
    fail: Option<(usize, bool)>,
    requests: Vec<String>,
}
impl FaultTransaction {
    fn new(inner: MemoryTransaction, fail: Option<(usize, bool)>) -> Self {
        Self {
            inner,
            actions: 0,
            fail,
            requests: Vec::new(),
        }
    }
    fn before(&mut self) -> Result<()> {
        self.actions += 1;
        if self.fail == Some((self.actions, false)) {
            bail!("before staged mutation")
        }
        Ok(())
    }
    fn after(&self) -> Result<()> {
        if self.fail == Some((self.actions, true)) {
            bail!("after staged mutation")
        }
        Ok(())
    }
}
impl Kv for FaultTransaction {
    async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(key).await
    }
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.before()?;
        self.inner.put(key, value).await?;
        self.after()
    }
}
impl Transaction for FaultTransaction {
    async fn execute(&mut self, op: &Operation) -> Result<Resolution> {
        self.before()?;
        self.requests.push(op.id().into());
        let result = self.inner.execute(op).await?;
        self.after()?;
        Ok(result)
    }
    async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<()> {
        self.before()?;
        self.inner.acknowledge(id, outcome, now).await?;
        self.after()
    }
    async fn abandon(&mut self, run: &str) -> Result<()> {
        self.before()?;
        self.inner.abandon(run).await?;
        self.after()
    }
}

#[test]
fn atomic_commit_couples_snapshot_state_and_effects_without_replay() -> Result<()> {
    let programs = programs(FLOW);
    let initial = MemoryTransaction::default();
    for complete in [false, true] {
        let (committed, op) = if complete {
            let mut e = Engine::new(initial.clone(), programs.clone());
            let view = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
            (
                e.into_storage()?,
                Some(view.state.pending[0].id().to_owned()),
            )
        } else {
            (initial.clone(), None)
        };
        let apply = |e: &mut Engine<FaultTransaction>| -> Result<View> {
            match &op {
                Some(id) => {
                    futures_executor::block_on(e.complete("r", id, Outcome::success(42), 1001))
                }
                None => futures_executor::block_on(e.start("r", "v1", Value::Null, 1000)),
            }
        };
        let mut trial = Engine::new(
            FaultTransaction::new(committed.clone(), None),
            programs.clone(),
        );
        let expected = apply(&mut trial)?;
        let action_count = trial.storage().actions;
        assert!(action_count >= 6);
        for after in [false, true] {
            for action in 1..=action_count {
                let before = image(&committed);
                let mut e = Engine::new(
                    FaultTransaction::new(committed.clone(), Some((action, after))),
                    programs.clone(),
                );
                assert!(
                    apply(&mut e).is_err(),
                    "complete={complete}, action={action}, after={after}"
                );
                let actions = e.storage().actions;
                assert!(apply(&mut e).is_err()); // no reuse of the partial turn
                assert_eq!(e.storage().actions, actions);
                assert!(e.into_storage().is_err()); // cannot extract for commit
                assert_eq!(image(&committed), before);
                let mut retry = Engine::new(
                    FaultTransaction::new(committed.clone(), None),
                    programs.clone(),
                );
                let view = apply(&mut retry)?; // explicit host redelivery, not recover replay
                assert_eq!(view.state, expected.state);
                let published = retry.into_storage()?.inner; // atomic host commit
                assert_eq!(published.state["n"], json!(if complete { 42 } else { 1 }));
                assert_eq!(published.effects.len(), 1);
                assert!(published.effects.contains_key(view.state.pending[0].id()));
                assert!(!published.values.keys().any(|k| k.contains("/event/")));
                let mut read_only = Engine::new(
                    FaultTransaction::new(published, Some((1, false))),
                    programs.clone(),
                );
                assert_eq!(
                    futures_executor::block_on(read_only.recover("r"))?.checkpoint_id,
                    view.checkpoint_id
                );
                assert_eq!(read_only.storage().actions, 0);
            }
        }
    }
    Ok(())
}

#[test]
fn committed_result_consumption_is_not_repeated_after_lost_commit_ack() -> Result<()> {
    let programs = programs(FLOW);
    let mut e = Engine::new(MemoryTransaction::default(), programs.clone());
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    let committed = e.into_storage()?;
    let mut e = Engine::new(committed.clone(), programs.clone());
    let next = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[0].id(),
        Outcome::success(42),
        1001,
    ))?;
    let committed = e.into_storage()?; // commit succeeds, host loses its acknowledgement
    assert_eq!(committed.state["n"], 42);
    assert_eq!(committed.effects.len(), 1);
    let mut e = Engine::new(
        FaultTransaction::new(committed, Some((1, false))),
        programs.clone(),
    );
    assert_eq!(
        futures_executor::block_on(e.recover("r"))?.state,
        next.state
    );
    let duplicate = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[0].id(),
        Outcome::success(999),
        1002,
    ))?;
    assert_eq!(duplicate.checkpoint_id, next.checkpoint_id);
    assert_eq!(e.storage().actions, 0);
    Ok(())
}

#[test]
fn aborted_read_needs_no_receipt_and_can_observe_new_transaction_state() -> Result<()> {
    let programs = programs(
        "async function main(_,ctx){const n=await ctx.kv.get('n');return await ctx.rpc.call({service:'Echo',handler:'echo'},n);}",
    );
    let mut committed = MemoryTransaction::default();
    committed.state.insert("n".into(), json!(7));
    let mut attempt = Engine::new(committed.clone(), programs.clone());
    let before = futures_executor::block_on(attempt.start("r", "v1", Value::Null, 1000))?;
    assert!(matches!(&before.state.pending[0],Operation::Rpc{input,..} if *input==json!(7)));
    drop(attempt); // no effect or snapshot was published
    committed.state.insert("n".into(), json!(99));
    let mut retry = Engine::new(committed, programs.clone());
    let after = futures_executor::block_on(retry.start("r", "v1", Value::Null, 1001))?;
    assert_eq!(before.state.pending[0].id(), after.state.pending[0].id());
    assert!(matches!(&after.state.pending[0],Operation::Rpc{input,..} if *input==json!(99)));
    Ok(())
}

#[test]
fn completing_one_fanout_effect_does_not_reemit_other_effects() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){return await Promise.all([
        ctx.rpc.call({service:'Echo',handler:'echo'},1),
        ctx.rpc.call({service:'Echo',handler:'echo'},2)]);}"#,
    );
    let initial = MemoryTransaction::default();
    let mut e = Engine::new(initial, programs.clone());
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(start.state.pending.len(), 2);
    let mut e = Engine::new(
        FaultTransaction::new(e.into_storage()?, None),
        programs.clone(),
    );
    futures_executor::block_on(e.complete("r", "r/1", Outcome::success(2), 1001))?;
    assert!(e.storage().requests.is_empty());
    assert_eq!(e.storage().inner.effects.len(), 1);
    let mut e = Engine::new(e.into_storage()?.inner, programs.clone());
    assert_eq!(
        value(&futures_executor::block_on(e.complete(
            "r",
            "r/0",
            Outcome::success(1),
            1002
        ))?),
        json!([1, 2])
    );
    assert!(e.storage().effects.is_empty());
    Ok(())
}

#[test]
fn host_checkpoint_stops_local_kv_chain_until_host_wakes_it() -> Result<()> {
    let programs = programs(
        "async function main(_,ctx){await ctx.kv.set('n',1);await ctx.kv.set('n',2);return await ctx.kv.get('n');}",
    );
    let mut e = Engine::new(MemoryTransaction::default(), programs.clone())
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    let first = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(e.storage().state["n"], 1);
    assert!(matches!(first.state.pending[0], Operation::Resume { .. }));
    let mut e = Engine::new(e.into_storage()?, programs.clone());
    let done = futures_executor::block_on(e.complete(
        "r",
        first.state.pending[0].id(),
        Outcome::success(Value::Null),
        1001,
    ))?;
    assert_eq!(value(&done), 2);
    assert_eq!(done.checkpoint_sequence, 1);
    assert!(e.storage().effects.is_empty());
    Ok(())
}

#[test]
fn guest_api_has_no_checkpoint_and_host_pause_preserves_queued_results() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        if ('checkpoint' in ctx || 'checkpoint' in durable) throw Error('guest checkpoint API');
        const rpc=ctx.rpc.call({service:'Remote',handler:'read'});
        const n=await ctx.kv.get('n');
        const trace=[n];
        await Promise.resolve();
        trace.push(n+1);
        return {n,trace,remote:await rpc};
    }"#,
    );
    let mut committed = MemoryTransaction::default();
    committed.state.insert("n".into(), json!(7));
    let mut e = Engine::new(committed, programs.clone())
        .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap());
    let first = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    let rpc = first
        .state
        .pending
        .iter()
        .find(|p| matches!(p, Operation::Rpc { .. }))
        .unwrap()
        .id()
        .to_owned();
    let wakeup = first
        .state
        .pending
        .iter()
        .find(|p| matches!(p, Operation::Resume { .. }))
        .unwrap()
        .id()
        .to_owned();
    let mut committed = e.into_storage()?;
    // Another committed application update must not replace the read result
    // already captured inside the pending QuickJS job queue.
    committed.state.insert("n".into(), json!(99));
    let mut e = Engine::new(FaultTransaction::new(committed, None), programs.clone());
    assert_eq!(
        futures_executor::block_on(e.recover("r"))?.state,
        first.state
    );
    let waiting =
        futures_executor::block_on(e.complete("r", &wakeup, Outcome::success(Value::Null), 1001))?;
    assert!(
        e.storage().requests.is_empty(),
        "must not repeat the read or RPC"
    );
    assert_eq!(waiting.state.pending.len(), 1);
    assert_eq!(waiting.state.pending[0].id(), rpc);
    let committed = e.into_storage()?.inner;
    let mut e = Engine::new(FaultTransaction::new(committed, None), programs.clone());
    let duplicate =
        futures_executor::block_on(e.complete("r", &wakeup, Outcome::success(Value::Null), 1002))?;
    assert_eq!(duplicate.checkpoint_id, waiting.checkpoint_id);
    assert_eq!(e.storage().actions, 0);
    let done = futures_executor::block_on(e.complete("r", &rpc, Outcome::success(42), 1003))?;
    assert_eq!(value(&done), json!({"n":7,"trace":[7,8],"remote":42}));
    assert!(e.storage().inner.effects.is_empty());
    Ok(())
}

#[test]
fn a_trap_ends_the_run_in_the_same_staged_transaction() -> Result<()> {
    let programs = programs(
        "async function main(_,ctx){await ctx.kv.set('n',1);ctx.rpc.call({service:'Echo',handler:'echo'});await ctx.kv.set('n',2);while(true){} }",
    );
    let committed = MemoryTransaction::default();
    let before = image(&committed);
    let mut e = Engine::new(committed.clone(), programs.clone());
    let failed = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(failed.state.status, Status::Failed);
    assert_eq!(e.storage().state["n"], 2); // staged, as for a thrown error
    assert!(e.storage().effects.is_empty()); // abandoned
    drop(e); // nothing is visible until the host publishes
    assert_eq!(image(&committed), before);
    assert!(
        futures_executor::block_on(Engine::new(committed, programs.clone()).recover("r")).is_err()
    );
    Ok(())
}

#[test]
fn custom_rust_function_and_guest_errors_resolve_inside_the_transaction() -> Result<()> {
    struct Local(MemoryTransaction);
    impl Kv for Local {
        async fn get(&mut self, k: &str) -> Result<Option<Vec<u8>>> {
            self.0.get(k).await
        }
        async fn put(&mut self, k: &str, v: &[u8]) -> Result<()> {
            self.0.put(k, v).await
        }
    }
    impl Transaction for Local {
        async fn execute(&mut self, op: &Operation) -> Result<Resolution> {
            match op {
                Operation::Call { name, args, .. } if name == "double" => Ok(Resolution::Ready(
                    Outcome::success(args.as_i64().unwrap() * 2),
                )),
                Operation::Rpc { .. } => Ok(Resolution::Ready(Outcome::failure(
                    "NotFound",
                    "unknown target",
                ))),
                _ => self.0.execute(op).await,
            }
        }
        async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<()> {
            self.0.acknowledge(id, outcome, now).await
        }
        async fn abandon(&mut self, run: &str) -> Result<()> {
            self.0.abandon(run).await
        }
    }
    let programs = programs(
        "async function main(_,ctx){const n=await ctx.call('double',21);try{await ctx.rpc.call({service:'Missing',handler:'x'});}catch(e){return {n,error:e.name};}}",
    );
    let mut e = Engine::new(Local(MemoryTransaction::default()), programs.clone());
    let done = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(value(&done), json!({"n":42,"error":"NotFound"}));
    assert_eq!(done.checkpoint_sequence, 0);
    assert!(e.storage().0.effects.is_empty());
    Ok(())
}

#[test]
fn invalid_guest_bindings_do_not_stage_operations() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        const bad=[()=>ctx.kv.get(7),()=>ctx.kv.set(null,1),()=>ctx.kv.delete({}),
          ()=>ctx.kv.set('x',undefined),()=>ctx.kv.set('x',new Map()),
          ()=>ctx.rpc.call(null),()=>ctx.rpc.call({service:'',handler:'x'}),
          ()=>ctx.rpc.call({service:'x',handler:'x',key:7}),
          ()=>ctx.rpc.call({service:'x',handler:'x',typo:true}),
          ()=>ctx.rpc.call({service:'x',handler:'x'},()=>1),
          ()=>ctx.kv.set('x','\ud800'),()=>ctx.kv.get('\ud800'),
          ()=>ctx.kv.set('x',{['\ud800']:1}),()=>ctx.call('\ud800')];
        return bad.map(f=>{try{f();return false;}catch(e){return e instanceof TypeError;}});
    }"#,
    );
    let initial = MemoryTransaction::default();
    let mut e = Engine::new(FaultTransaction::new(initial, None), programs.clone());
    let done = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(value(&done), json!(vec![true; 14]));
    assert_eq!(done.state.next_operation, 0);
    assert!(e.storage().requests.is_empty());
    Ok(())
}

#[test]
fn serialized_accessor_values_are_validated_before_staging() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        let reads=0;
        const value={get text(){return ++reads===1 ? 'valid' : '\ud800';}};
        try {await ctx.kv.set('bad',value);return false;}
        catch(e){return e instanceof TypeError;}
    }"#,
    );
    let initial = MemoryTransaction::default();
    let mut engine = Engine::new(FaultTransaction::new(initial, None), programs.clone());
    let done = futures_executor::block_on(engine.start("r", "v1", Value::Null, 1000))?;
    assert_eq!(value(&done), true);
    assert!(engine.storage().requests.is_empty());
    Ok(())
}

#[test]
fn timers_cannot_exceed_the_host_clock_range() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        try {await ctx.sleep(1);return false;}catch(e){return e instanceof RangeError;}
    }"#,
    );
    let initial = MemoryTransaction::default();
    let mut engine = Engine::new(FaultTransaction::new(initial, None), programs.clone());
    let done =
        futures_executor::block_on(engine.start("r", "v1", Value::Null, u64::MAX / 1_000_000))?;
    assert_eq!(value(&done), true);
    assert!(engine.storage().requests.is_empty());
    Ok(())
}

#[test]
fn rpc_delivery_before_resume_preserves_jobs_and_multiple_wakeups() -> Result<()> {
    let programs = programs(
        r#"async function main(_,ctx){
        const remote=ctx.rpc.call({service:'Echo',handler:'echo'});
        const n=await ctx.kv.get('n');
        await ctx.kv.set('n',n+1);
        return {n,remote:await remote};
    }"#,
    );
    let mut committed = MemoryTransaction::default();
    committed.state.insert("n".into(), json!(7));
    let make = |tx| {
        Engine::new(tx, programs.clone())
            .with_checkpoint_interval(std::num::NonZeroUsize::new(1).unwrap())
    };
    let mut e = make(committed);
    let first = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    assert!(matches!(first.state.pending[0], Operation::Rpc { .. }));
    assert!(matches!(first.state.pending[1], Operation::Resume { .. }));
    let mut e = make(e.into_storage()?);
    let second = futures_executor::block_on(e.complete(
        "r",
        first.state.pending[0].id(),
        Outcome::success(42),
        1001,
    ))?;
    assert_eq!(e.storage().state["n"], 8);
    assert_eq!(second.state.pending.len(), 2);
    assert!(
        second
            .state
            .pending
            .iter()
            .all(|op| matches!(op, Operation::Resume { .. }))
    );
    let mut e = make(e.into_storage()?);
    let waiting = futures_executor::block_on(e.complete(
        "r",
        second.state.pending[1].id(),
        Outcome::success(Value::Null),
        1002,
    ))?;
    assert_eq!(waiting.state.status, Status::Waiting);
    assert_eq!(
        waiting.state.outcome,
        Some(Outcome::success(json!({"n":7,"remote":42})))
    );
    let mut e = make(e.into_storage()?);
    let done = futures_executor::block_on(e.complete(
        "r",
        first.state.pending[1].id(),
        Outcome::success(Value::Null),
        1003,
    ))?;
    assert_eq!(done.state.status, Status::Completed);
    assert_eq!(done.state.outcome, waiting.state.outcome);
    assert_eq!(e.storage().state["n"], 8); // queued continuation ran exactly once
    assert!(e.storage().effects.is_empty());
    Ok(())
}

/// Storage that suspends before every read and write, as a remote store would:
/// the engine must hold no Wasm call while it waits.
#[test]
fn checkpoint_storage_may_suspend() -> Result<()> {
    struct Remote {
        inner: MemoryTransaction,
        suspended: usize,
    }
    /// Pending once, then ready: one trip back to the executor.
    struct Yield(bool);
    impl std::future::Future for Yield {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if std::mem::replace(&mut self.0, true) {
                return std::task::Poll::Ready(());
            }
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
    impl Kv for Remote {
        async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
            Yield(false).await;
            self.inner.get(key).await
        }
        async fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
            Yield(false).await;
            self.suspended += 1;
            self.inner.put(key, value).await
        }
    }
    impl Transaction for Remote {
        async fn execute(&mut self, op: &Operation) -> Result<Resolution> {
            self.inner.execute(op).await
        }
        async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64) -> Result<()> {
            self.inner.acknowledge(id, outcome, now).await
        }
        async fn abandon(&mut self, run: &str) -> Result<()> {
            self.inner.abandon(run).await
        }
    }
    let remote = |inner| Remote {
        inner,
        suspended: 0,
    };
    let programs = programs(FLOW);
    let mut e = Engine::new(remote(MemoryTransaction::default()), programs.clone());
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    let rpc = start.state.pending[0].id().to_owned();
    let mut e = Engine::new(remote(e.into_storage()?.inner), programs.clone());
    let next = futures_executor::block_on(e.complete("r", &rpc, Outcome::success(7), 1001))?;
    let audit = next.state.pending[0].id().to_owned();
    let done =
        futures_executor::block_on(e.complete("r", &audit, Outcome::success(Value::Null), 1002))?;
    assert_eq!(value(&done), json!(7));
    assert!(e.storage().suspended > 0);
    assert_eq!(
        futures_executor::block_on(e.recover("r"))?.checkpoint_id,
        done.checkpoint_id
    );
    Ok(())
}

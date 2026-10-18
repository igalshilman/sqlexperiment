use anyhow::Result;
use common::{programs, runtime};
use quickjs_durable::{
    Engine, MemoryTransaction, Operation, Outcome, Program, Programs, Runtime, Status,
};
use serde_json::{Value, json};
use std::sync::Arc;

mod common;

fn engine(source: &str) -> Engine<MemoryTransaction> {
    Engine::new(MemoryTransaction::default(), programs(source))
}
fn value(outcome: &Option<Outcome>) -> Value {
    match outcome {
        Some(Outcome::Success { ok: true, value }) => value.clone(),
        other => panic!("Unexpected outcome {other:?}"),
    }
}
const PROGRAM: &str = r#"async function main() {
  const state={n:40}; state.self=state;
  const map=new Map([['state',state]]);
  state.n+=await durable.call('increment',{by:2});
  await durable.call('observe');
  return {n:map.get('state').n,identity:state.self===state};
}"#;

#[test]
fn closure_cycles_map_and_cold_resume() -> Result<()> {
    let mut e = engine(PROGRAM);
    let start = futures_executor::block_on(e.start("run", "v1", Value::Null, 1000))?;
    assert_eq!(start.state.status, Status::Waiting);
    let mut e = Engine::new(e.into_storage()?, programs(PROGRAM));
    let step = futures_executor::block_on(e.complete(
        "run",
        start.state.pending[0].id(),
        Outcome::success(2),
        1001,
    ))?;
    let mut e = Engine::new(e.into_storage()?, programs(PROGRAM));
    let done = futures_executor::block_on(e.complete(
        "run",
        step.state.pending[0].id(),
        Outcome::success(Value::Null),
        1002,
    ))?;
    assert_eq!(value(&done.state.outcome), json!({"n":42,"identity":true}));
    assert_eq!(
        futures_executor::block_on(e.recover("run"))?.checkpoint_id,
        done.checkpoint_id
    );
    assert_eq!(
        futures_executor::block_on(e.complete("run", "run/0", Outcome::success(999), 1003))?
            .checkpoint_id,
        done.checkpoint_id
    );
    assert!(
        futures_executor::block_on(e.complete("run", "run/999", Outcome::success(1), 1004))
            .is_err()
    );
    Ok(())
}

#[test]
fn golden_versions_and_invocation_isolation() -> Result<()> {
    let source = "const shared={n:10}; async function main(input){ shared.n+=input; await durable.call('observe'); return shared.n; }";
    let programs = Arc::new(
        Programs::new(runtime())
            .compile("v1", source)?
            .compile("v2", "async function main(){return 99}")?,
    );
    let mut e = Engine::new(MemoryTransaction::default(), programs.clone());
    let a = futures_executor::block_on(e.start("a", "v1", json!(2), 1000))?;
    let b = futures_executor::block_on(e.start("b", "v1", json!(7), 1000))?;
    assert_eq!(
        value(
            &futures_executor::block_on(e.complete(
                "a",
                a.state.pending[0].id(),
                Outcome::success(Value::Null),
                1001
            ))?
            .state
            .outcome
        ),
        json!(12)
    );
    assert_eq!(
        value(
            &futures_executor::block_on(e.complete(
                "b",
                b.state.pending[0].id(),
                Outcome::success(Value::Null),
                1001
            ))?
            .state
            .outcome
        ),
        json!(17)
    );
    assert_eq!(
        value(
            &futures_executor::block_on(e.start("c", "v2", Value::Null, 1002))?
                .state
                .outcome
        ),
        json!(99)
    );
    let committed = e.into_storage()?;
    for (version, input) in [("v2", json!(2)), ("v1", json!(3))] {
        let mut invalid = Engine::new(committed.clone(), programs.clone());
        assert!(futures_executor::block_on(invalid.start("a", version, input, 1003)).is_err());
        assert!(invalid.into_storage().is_err());
    }
    Ok(())
}

/// A run continues only on the version of its program it started on: a
/// catalog whose v1 has another source refuses to load it.
#[test]
fn a_run_refuses_another_version_of_its_program() -> Result<()> {
    let source = "async function main(){await durable.call('observe');return 1}";
    let mut e = engine(source);
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let committed = e.into_storage()?;
    let changed = programs("async function main(){await durable.call('observe');return 2}");
    let error =
        futures_executor::block_on(Engine::new(committed.clone(), changed.clone()).recover("r"))
            .unwrap_err();
    assert!(
        format!("{error:#}").contains("another version of program v1"),
        "{error:#}"
    );
    let operation = start.state.pending[0].id();
    let mut e = Engine::new(committed.clone(), changed.clone());
    assert!(
        futures_executor::block_on(e.complete("r", operation, Outcome::success(Value::Null), 2))
            .is_err()
    );
    assert!(e.into_storage().is_err());
    let mut e = Engine::new(committed.clone(), changed);
    assert!(futures_executor::block_on(e.start("r", "v1", Value::Null, 1)).is_err());
    // Its own version still completes it.
    let mut e = Engine::new(committed, programs(source));
    let done =
        futures_executor::block_on(e.complete("r", operation, Outcome::success(Value::Null), 2))?;
    assert_eq!(value(&done.state.outcome), json!(1));
    Ok(())
}

/// A program evaluates to the same golden image on every runtime: a process
/// that starts later (here, a second runtime) continues another's runs, and
/// they end exactly as in one process alone.
#[test]
fn every_runtime_evaluates_a_program_alike() -> Result<()> {
    let compiled = Program::compile(&runtime(), "v1", PROGRAM)?;
    let again = Program::compile(&Arc::new(Runtime::new()?), "v1", PROGRAM)?;
    assert_eq!(again.version(), compiled.version());
    let again = Arc::new(Programs::new(runtime()).with(again)?);
    let compiled = Arc::new(Programs::new(runtime()).with(compiled)?);
    let run = |catalogs: [&Arc<Programs>; 3]| -> Result<(Value, MemoryTransaction)> {
        let mut e = Engine::new(MemoryTransaction::default(), catalogs[0].clone());
        let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
        let mut e = Engine::new(e.into_storage()?, catalogs[1].clone());
        let step = futures_executor::block_on(e.complete(
            "r",
            start.state.pending[0].id(),
            Outcome::success(2),
            2,
        ))?;
        let mut e = Engine::new(e.into_storage()?, catalogs[2].clone());
        let done = futures_executor::block_on(e.complete(
            "r",
            step.state.pending[0].id(),
            Outcome::success(Value::Null),
            3,
        ))?;
        Ok((value(&done.state.outcome), e.into_storage()?))
    };
    let (expected, expected_storage) = run([&compiled; 3])?;
    assert_eq!(expected, json!({"n":42,"identity":true}));
    for catalogs in [[&again; 3], [&compiled, &again, &compiled]] {
        let (result, storage) = run(catalogs)?;
        assert_eq!(result, expected);
        assert_eq!(storage.values, expected_storage.values);
    }
    Ok(())
}

#[test]
fn promise_race_all_any_settled_and_errors() -> Result<()> {
    let mut e = engine(
        r#"async function main(){
      const a=durable.call('a'), b=durable.call('b');
      const winner=await Promise.race([a,b]);
      const all=await Promise.all([a,b]);
      const outcomes=await Promise.allSettled([durable.call('fail'),Promise.resolve(8)]);
      const any=await Promise.any([Promise.reject('no'),Promise.resolve(9)]);
      return {winner,all,outcomes:outcomes.map(x=>x.status==='rejected'?{status:x.status,message:x.reason.message}:x),any};
    }"#,
    );
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    futures_executor::block_on(e.complete(
        "r",
        start.state.pending[1].id(),
        Outcome::success(2),
        1001,
    ))?;
    let state = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[0].id(),
        Outcome::success(1),
        1002,
    ))?;
    let done = futures_executor::block_on(e.complete(
        "r",
        state.state.pending[0].id(),
        Outcome::failure("Oops", "failed"),
        1003,
    ))?;
    let result = value(&done.state.outcome);
    assert_eq!(result["winner"], json!(2));
    assert_eq!(result["all"], json!([1, 2]));
    assert_eq!(result["outcomes"][0]["status"], "rejected");
    assert_eq!(result["any"], 9);
    Ok(())
}

#[test]
fn a_run_ends_when_main_settles() -> Result<()> {
    let mut e = engine(
        "async function main(){const a=durable.call('a'), b=durable.call('b').catch(()=>durable.call('after'));return await Promise.race([a,b]);}",
    );
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    assert_eq!(start.state.pending.len(), 2);
    let done = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[0].id(),
        Outcome::success(7),
        2,
    ))?;
    // The loser is dropped: nothing waits on it, and its continuation never runs.
    assert_eq!(done.state.status, Status::Completed);
    assert_eq!(value(&done.state.outcome), 7);
    assert!(done.state.pending.is_empty());
    assert!(
        e.storage().effects.is_empty(),
        "no request is left outstanding"
    );
    // Its late result changes nothing.
    let late = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[1].id(),
        Outcome::failure("Error", "loser"),
        3,
    ))?;
    assert_eq!(late.checkpoint_id, done.checkpoint_id);
    Ok(())
}

#[test]
fn logical_time_prng_and_timer() -> Result<()> {
    let source = "async function main(){ const a=[Date.now(),new Date().getTimezoneOffset(),Math.random()]; await durable.sleep(50); return [a,Date.now(),Math.random()]; }";
    let mut a = engine(source);
    let mut b = engine(source);
    let start = futures_executor::block_on(a.start("same", "v1", Value::Null, 1000))?;
    futures_executor::block_on(b.start("same", "v1", Value::Null, 1000))?;
    let committed = a.into_storage()?;
    let mut a = Engine::new(committed.clone(), programs(source));
    assert!(
        futures_executor::block_on(a.complete(
            "same",
            start.state.pending[0].id(),
            Outcome::success(Value::Null),
            1049
        ))
        .is_err()
    );
    assert!(a.into_storage().is_err());
    let mut a = Engine::new(committed, programs(source));
    let x = futures_executor::block_on(a.complete(
        "same",
        start.state.pending[0].id(),
        Outcome::success(Value::Null),
        1050,
    ))?;
    let y = futures_executor::block_on(b.complete(
        "same",
        start.state.pending[0].id(),
        Outcome::success(Value::Null),
        1050,
    ))?;
    assert_eq!(x.state.outcome, y.state.outcome);
    let result = value(&x.state.outcome);
    assert_eq!(result[0][0], 1000);
    assert_eq!(result[0][1], 0);
    assert_eq!(result[1], 1050);
    Ok(())
}

#[test]
fn zero_growth_overwrites_and_no_history_dependency() -> Result<()> {
    let source = "async function main(){await durable.call('observe'); const b=new Uint8Array(3*1024*1024); b[0]=7;b[b.length-1]=9;await durable.call('observe');b[0]=0;await durable.call('observe');return [b[0],b[b.length-1],b[4096]];}";
    let mut e = engine(source);
    let mut state = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let initial = state.image_bytes;
    state = futures_executor::block_on(e.complete(
        "r",
        state.state.pending[0].id(),
        Outcome::success(Value::Null),
        2,
    ))?;
    assert!(state.image_bytes >= initial + 3 * 1024 * 1024);
    assert!(state.new_blob_bytes < 100_000);
    state = futures_executor::block_on(e.complete(
        "r",
        state.state.pending[0].id(),
        Outcome::success(Value::Null),
        3,
    ))?;
    // Only the head's manifest is kept: it alone restores the run.
    let kv = e.into_storage()?;
    assert_eq!(
        kv.values
            .keys()
            .filter(|key| key.starts_with("checkpoint/"))
            .count(),
        1
    );
    let mut e = Engine::new(kv, programs(source));
    assert_eq!(
        futures_executor::block_on(e.recover("r"))?.checkpoint_id,
        state.checkpoint_id
    );
    let done = futures_executor::block_on(e.complete(
        "r",
        state.state.pending[0].id(),
        Outcome::success(Value::Null),
        4,
    ))?;
    assert_eq!(value(&done.state.outcome), json!([0, 9, 0]));
    Ok(())
}

/// Recovery needs the run's program in the catalog, and a checkpoint of this
/// build of the runtime.
#[test]
fn a_missing_program_and_another_build_are_rejected() -> Result<()> {
    let mut e = engine(PROGRAM);
    futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let committed = e.into_storage()?;
    let renamed = Arc::new(Programs::new(runtime()).compile("v2", PROGRAM)?);
    assert!(
        futures_executor::block_on(Engine::new(committed.clone(), renamed).recover("r")).is_err()
    );
    let mut rebuilt = committed.clone();
    for (key, manifest) in &mut rebuilt.values {
        if key.starts_with("checkpoint/") {
            let mut json: Value = serde_json::from_slice(manifest)?;
            json["runtime"] = json!(0);
            *manifest = serde_json::to_vec(&json)?;
        }
    }
    assert!(
        futures_executor::block_on(Engine::new(rebuilt, programs(PROGRAM)).recover("r")).is_err()
    );
    futures_executor::block_on(Engine::new(committed, programs(PROGRAM)).recover("r"))?;
    Ok(())
}

#[test]
fn initialization_and_guest_failure_contract() -> Result<()> {
    for source in [
        "durable.call('forbidden');async function main(){}",
        "async function main( {",
        "async function notMain(){}",
    ] {
        assert!(Programs::new(runtime()).compile("bad", source).is_err());
    }
    let main = "async function main(){}";
    assert!(Program::compile(&runtime(), "not a name", main).is_err());
    assert!(
        Programs::new(runtime())
            .compile("v1", main)?
            .compile("v1", main)
            .is_err()
    );
    for source in [
        "async function main(){throw new Error('boom')}",
        "async function main(){return ()=>1}",
        "async function main(){await new Promise(()=>{})}",
    ] {
        let mut e = engine(source);
        assert_eq!(
            futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?
                .state
                .status,
            Status::Failed
        );
    }
    let mut e = engine("async function main(){return undefined}");
    assert_eq!(
        value(
            &futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?
                .state
                .outcome
        ),
        Value::Null
    );
    Ok(())
}

/// A synchronous job longer than a turn's fuel traps, and would trap again on
/// every retry: the engine ends the run instead, over its last good image,
/// abandoning its outstanding requests.
#[test]
fn a_trap_ends_the_run() -> Result<()> {
    let source = "async function main(_,ctx){await ctx.kv.set('before',1);const a=ctx.rpc.call({service:'S',handler:'h'});await durable.call('observe');await ctx.kv.set('after',1);while(true){}}";
    let mut e = engine(source);
    let before = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    assert_eq!(before.state.pending.len(), 2);
    let committed = e.into_storage()?;
    let mut e = Engine::new(committed, programs(source));
    let failed = futures_executor::block_on(e.complete(
        "r",
        before.state.pending[1].id(),
        Outcome::success(Value::Null),
        2,
    ))?;
    assert_eq!(failed.state.status, Status::Failed);
    assert!(failed.state.pending.is_empty());
    let Some(Outcome::Failure { error, .. }) = &failed.state.outcome else {
        panic!("Expected a failure: {:?}", failed.state.outcome);
    };
    assert_eq!(error.name, "Trap");
    assert!(error.message.contains("fuel"), "{}", error.message);
    // The writes before the trap stand, as for a thrown error; the requests
    // still outstanding are abandoned.
    assert_eq!(e.storage().state["after"], json!(1));
    assert!(e.storage().effects.is_empty());
    let committed = e.into_storage()?;
    // A late result for an abandoned request changes nothing.
    let mut e = Engine::new(committed, programs(source));
    let late = futures_executor::block_on(e.complete(
        "r",
        before.state.pending[0].id(),
        Outcome::success(1),
        3,
    ))?;
    assert_eq!(late.checkpoint_id, failed.checkpoint_id);
    assert_eq!(
        futures_executor::block_on(e.recover("r"))?.state.status,
        Status::Failed
    );
    Ok(())
}

/// A trap in the first turn ends the run over the golden image.
#[test]
fn a_trap_in_the_first_turn_ends_the_run() -> Result<()> {
    let source = "async function main(){while(true){}}";
    let mut e = engine(source);
    let failed = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    assert_eq!(failed.state.status, Status::Failed);
    assert_eq!(failed.checkpoint_sequence, 0);
    let mut e = Engine::new(e.into_storage()?, programs(source));
    let again = futures_executor::block_on(e.start("r", "v1", Value::Null, 2))?;
    assert_eq!(again.checkpoint_id, failed.checkpoint_id);
    Ok(())
}

#[test]
fn stack_limit_and_unicode_survive_cold_restore() -> Result<()> {
    let source = r#"function overflow(){
        function recurse(){return recurse()+1;}
        try{recurse();return false;}catch(e){return /stack/i.test(e.message);}
    }
    async function main(){
        const before=overflow();
        const text='nul:\u0000 lone:\ud800 snow:\u96ea';
        await durable.call('observe');
        return {before,after:overflow(),nul:text.charCodeAt(4),surrogate:text.charCodeAt(11),snow:text.endsWith('雪')};
    }"#;
    let mut e = engine(source);
    let first = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    let mut e = Engine::new(e.into_storage()?, programs(source));
    let done = futures_executor::block_on(e.complete(
        "r",
        first.state.pending[0].id(),
        Outcome::success(Value::Null),
        1001,
    ))?;
    // JSON cannot decode lone surrogates into Rust strings. Test them as code
    // units inside the guest instead of requiring an invalid Unicode scalar.
    assert_eq!(value(&done.state.outcome)["before"], true);
    assert_eq!(value(&done.state.outcome)["after"], true);
    assert_eq!(value(&done.state.outcome)["nul"], 0);
    assert_eq!(value(&done.state.outcome)["surrogate"], 0xd800);
    assert_eq!(value(&done.state.outcome)["snow"], true);
    Ok(())
}

#[test]
fn zero_overrides_nonzero_golden_and_return_to_golden_removes_pages() -> Result<()> {
    let mut e = engine(
        "const bytes=new Uint8Array(128*1024).fill(5);async function main(){await durable.call('observe');bytes.fill(0);await durable.call('observe');const wasZero=bytes[65536]===0;bytes.fill(5);await durable.call('observe');return [wasZero,bytes[65536]];}",
    );
    let a = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let b = futures_executor::block_on(e.complete(
        "r",
        a.state.pending[0].id(),
        Outcome::success(Value::Null),
        2,
    ))?;
    assert!(b.override_pages > a.override_pages + 20);
    let c = futures_executor::block_on(e.complete(
        "r",
        b.state.pending[0].id(),
        Outcome::success(Value::Null),
        3,
    ))?;
    assert!(c.override_pages + 20 < b.override_pages);
    let done = futures_executor::block_on(e.complete(
        "r",
        c.state.pending[0].id(),
        Outcome::success(Value::Null),
        4,
    ))?;
    assert_eq!(value(&done.state.outcome), json!([true, 5]));
    Ok(())
}

/// A long run of local requests: more than one turn's fuel. The engine
/// checkpoints at a safe boundary once half the fuel is spent, and a Resume
/// wakeup continues it, so the run finishes instead of trapping every turn.
#[test]
fn long_local_loops_checkpoint_before_fuel_runs_out() -> Result<()> {
    let mut e = engine(
        "async function main(_,ctx){for(let i=0;i<2000;i++){await ctx.kv.set('k',i);}return await ctx.kv.get('k');}",
    );
    let mut view = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let mut turns = 1;
    while view.state.status == Status::Waiting {
        let [Operation::Resume { id }] = &view.state.pending[..] else {
            panic!("Expected one Resume wakeup: {:?}", view.state.pending);
        };
        let id = id.clone();
        view = futures_executor::block_on(e.complete("r", &id, Outcome::success(Value::Null), 2))?;
        turns += 1;
    }
    assert_eq!(value(&view.state.outcome), json!(1999));
    assert!(turns > 2, "{turns} turns");
    Ok(())
}

/// Restore reads every batch the manifest references. A long run would read
/// more of them every turn; past a limit, a turn rewrites every live page into
/// its own batch, so the count stays bounded and restoring stays exact.
#[test]
fn long_runs_reference_a_bounded_number_of_batches() -> Result<()> {
    let mut e = engine(
        "const log=[];async function main(_,ctx){for(let i=0;i<60;i++){log.push('x'.repeat(5000)+i);await durable.call('tick');}return log.length+':'+log[59].slice(-2);}",
    );
    let mut view = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let mut most = 0;
    while view.state.status == Status::Waiting {
        let id = view.state.pending[0].id().to_owned();
        view = futures_executor::block_on(e.complete("r", &id, Outcome::success(Value::Null), 2))?;
        let manifest: Value = serde_json::from_slice(
            &e.storage().values[&format!("checkpoint/{}", view.checkpoint_id)],
        )?;
        let batches: std::collections::BTreeSet<u64> = manifest["pages"]
            .as_object()
            .unwrap()
            .values()
            .map(|reference| reference["b"].as_u64().unwrap())
            .collect();
        most = most.max(batches.len());
        // Only the head's manifest and the batches it references are kept.
        assert_eq!(stored(&e, "blob/"), batches.len());
        assert_eq!(stored(&e, "checkpoint/"), 1);
    }
    assert_eq!(value(&view.state.outcome), json!("60:59"));
    assert!(most <= 16, "{most} batches");
    assert!(most > 1);
    // A settled run keeps its outcome, not its image.
    assert_eq!(stored(&e, "blob/"), 0);
    Ok(())
}

fn stored(e: &Engine<MemoryTransaction>, prefix: &str) -> usize {
    e.storage()
        .values
        .keys()
        .filter(|key| key.starts_with(prefix))
        .count()
}

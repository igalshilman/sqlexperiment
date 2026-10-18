//! Runs, turn by turn, over a store in memory: what survives a wait, what a
//! turn commits, and what a new attempt restores.
use anyhow::Result;
use common::*;
use quickjs_durable::{MemoryStore, Operation, Outcome, Program, Programs, Run, Runtime, Status};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

mod common;

const PROGRAM: &str = r#"async function main() {
  const state={n:40}; state.self=state;
  const map=new Map([['state',state]]);
  state.n+=await durable.call('increment',{by:2});
  await durable.call('observe');
  return {n:map.get('state').n,identity:state.self===state};
}"#;

#[test]
fn a_new_attempt_restores_closures_cycles_and_maps() -> Result<()> {
    let store = MemoryStore::default();
    let mut run = start(&store, PROGRAM, Value::Null, 1000)?;
    assert_eq!(run.view().state.status, Status::Waiting);
    let first = run.view().state.pending[0].id().to_owned();
    block(run.deliver(&store, &first, Outcome::success(2), 1001))?;
    // Another attempt: the VM is restored from the commit.
    let mut run = start(&store, PROGRAM, Value::Null, 1002)?;
    let second = run.view().state.pending[0].id().to_owned();
    let done = block(run.deliver(&store, &second, Outcome::success(Value::Null), 1002))?;
    assert_eq!(value(&done), json!({"n":42,"identity":true}));
    // A request delivered before changes nothing; an unknown one is refused.
    let again = block(run.deliver(&store, &first, Outcome::success(999), 1003))?;
    assert_eq!(again.turns, done.turns);
    assert!(block(run.deliver(&store, "r/999", Outcome::success(1), 1004)).is_err());
    Ok(())
}

/// A live VM, kept from turn to turn, and one restored at every turn end
/// alike, down to the bytes they store.
#[test]
fn a_live_run_and_a_restored_one_end_alike() -> Result<()> {
    let source = r#"async function main(_,ctx){
      const log=[];
      for(let i=0;i<5;i++){log.push(await ctx.rpc.call({service:'S',handler:'h'},i));await ctx.kv.set('i',i);}
      await ctx.sleep(10);
      return log.map(x=>x.input);
    }"#;
    let live = MemoryStore::default();
    let mut run = start(&live, source, Value::Null, 1)?;
    let expected = finish(&mut run, &live)?;
    let restored = MemoryStore::default();
    start(&restored, source, Value::Null, 1)?;
    loop {
        let mut run = start(&restored, source, Value::Null, 1)?;
        if run.view().state.outcome.is_some() {
            assert_eq!(run.view().state, expected.state);
            break;
        }
        step(&mut run, &restored)?;
    }
    assert_eq!(value(&expected), json!([0, 1, 2, 3, 4]));
    assert_eq!(restored.values(), live.values());
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
    let run = |catalogs: [&Arc<Programs>; 3]| -> Result<MemoryStore> {
        let store = MemoryStore::default();
        for catalog in catalogs {
            let mut run = block(Run::start(catalog, &store, "r", "v1", Value::Null, 1))?;
            if run.view().state.outcome.is_none() {
                step(&mut run, &store)?;
            }
        }
        Ok(store)
    };
    let expected = run([&compiled; 3])?;
    for catalogs in [[&again; 3], [&compiled, &again, &compiled]] {
        assert_eq!(run(catalogs)?.values(), expected.values());
    }
    Ok(())
}

/// A run continues only on the version of its program it started on, and on
/// the version of the runtime.
#[test]
fn a_run_refuses_another_version_of_its_program_or_runtime() -> Result<()> {
    let source = "async function main(){await durable.call('observe');return 1}";
    let store = MemoryStore::default();
    start(&store, source, Value::Null, 1)?;
    let changed = "async function main(){await durable.call('observe');return 2}";
    let error = start(&store, changed, Value::Null, 1).err().unwrap();
    assert!(
        format!("{error:#}").contains("another version of program v1"),
        "{error:#}"
    );
    let renamed = Arc::new(Programs::new(runtime()).compile("v2", source)?);
    assert!(block(Run::start(&renamed, &store, "r", "v1", Value::Null, 1)).is_err());
    let rebuilt = MemoryStore::default();
    let mut commit: Value = serde_json::from_slice(&store.values()["commit"])?;
    commit["runtime"] = json!(0);
    block(quickjs_durable::Store::put(
        &rebuilt,
        "commit",
        serde_json::to_vec(&commit)?,
    ))?;
    assert!(start(&rebuilt, source, Value::Null, 1).is_err());
    // Its own version still completes it.
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(value(&finish(&mut run, &store)?), json!(1));
    Ok(())
}

#[test]
fn promise_race_all_any_settled_and_errors() -> Result<()> {
    let source = r#"async function main(){
      const a=durable.call('a'), b=durable.call('b');
      const winner=await Promise.race([a,b]);
      const all=await Promise.all([a,b]);
      const outcomes=await Promise.allSettled([durable.call('fail'),Promise.resolve(8)]);
      const any=await Promise.any([Promise.reject('no'),Promise.resolve(9)]);
      return {winner,all,outcomes:outcomes.map(x=>x.status==='rejected'?{status:x.status,message:x.reason.message}:x),any};
    }"#;
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1000)?;
    let pending = run.view().state.pending;
    block(run.deliver(&store, pending[1].id(), Outcome::success(2), 1001))?;
    let state = block(run.deliver(&store, pending[0].id(), Outcome::success(1), 1002))?.state;
    let failure = Outcome::failure("Oops", "failed");
    let done = block(run.deliver(&store, state.pending[0].id(), failure, 1003))?;
    let result = value(&done);
    assert_eq!(result["winner"], json!(2));
    assert_eq!(result["all"], json!([1, 2]));
    assert_eq!(result["outcomes"][0]["status"], "rejected");
    assert_eq!(result["any"], 9);
    Ok(())
}

#[test]
fn a_run_ends_when_main_settles() -> Result<()> {
    let source = "async function main(_,ctx){const a=durable.call('a'), b=durable.call('b').catch(()=>ctx.kv.set('late',1));return await Promise.race([a,b]);}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let pending = run.view().state.pending;
    assert_eq!(pending.len(), 2);
    let done = block(run.deliver(&store, pending[0].id(), Outcome::success(7), 2))?;
    // The loser is dropped: nothing waits on it, and its continuation never runs.
    assert_eq!(done.state.status, Status::Completed);
    assert_eq!(value(&done), 7);
    assert!(done.state.pending.is_empty());
    let late = block(run.deliver(&store, pending[1].id(), Outcome::failure("E", "x"), 3))?;
    assert_eq!(late.turns, done.turns);
    assert_eq!(kv(&store, "late"), None);
    Ok(())
}

#[test]
fn logical_time_seeded_random_and_timers() -> Result<()> {
    let source = "async function main(){ const a=[Date.now(),new Date().getTimezoneOffset(),Math.random()]; await durable.sleep(50); return [a,Date.now(),Math.random()]; }";
    let (a, b) = (MemoryStore::default(), MemoryStore::default());
    let mut x = start(&a, source, Value::Null, 1000)?;
    let mut y = start(&b, source, Value::Null, 1000)?;
    let timer = x.view().state.pending[0].id().to_owned();
    assert!(block(x.deliver(&a, &timer, Outcome::success(Value::Null), 1049)).is_err());
    let x = block(x.deliver(&a, &timer, Outcome::success(Value::Null), 1050))?;
    let y = block(y.deliver(&b, &timer, Outcome::success(Value::Null), 1050))?;
    assert_eq!(x.state.outcome, y.state.outcome);
    let result = value(&x);
    assert_eq!(result[0][0], 1000);
    assert_eq!(result[0][1], 0);
    assert_eq!(result[1], 1050);
    Ok(())
}

#[test]
fn memory_growth_survives_a_restore() -> Result<()> {
    let source = "async function main(){await durable.call('observe'); const b=new Uint8Array(3*1024*1024); b[0]=7;b[b.length-1]=9;await durable.call('observe');b[0]=0;await durable.call('observe');return [b[0],b[b.length-1],b[4096]];}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let initial = run.view().memory_bytes;
    let grown = step(&mut run, &store)?;
    assert!(grown.memory_bytes >= initial + 3 * 1024 * 1024);
    assert!(grown.batch_bytes < 100_000);
    step(&mut run, &store)?;
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(value(&finish(&mut run, &store)?), json!([0, 9, 0]));
    Ok(())
}

#[test]
fn programs_are_checked_and_failures_end_runs() -> Result<()> {
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
        let run = start(&MemoryStore::default(), source, Value::Null, 1)?;
        assert_eq!(run.view().state.status, Status::Failed, "{source}");
    }
    let run = start(
        &MemoryStore::default(),
        "async function main(){}",
        Value::Null,
        1,
    )?;
    assert_eq!(value(&run.view()), Value::Null);
    Ok(())
}

/// A synchronous job longer than a turn's fuel traps, and would trap again:
/// the run ends instead. The KV writes before the trap stand, as for a thrown
/// error; its requests still outstanding are dropped.
#[test]
fn a_trap_ends_the_run() -> Result<()> {
    let source = "async function main(_,ctx){await ctx.kv.set('before',1);ctx.rpc.call({service:'S',handler:'h'});await durable.call('observe');await ctx.kv.set('after',1);while(true){}}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let pending = run.view().state.pending;
    assert_eq!(pending.len(), 2);
    let failed = block(run.deliver(&store, pending[1].id(), Outcome::success(Value::Null), 2))?;
    assert_eq!(failed.state.status, Status::Failed);
    assert!(failed.state.pending.is_empty());
    let Some(Outcome::Failure { error, .. }) = &failed.state.outcome else {
        panic!("Expected a failure: {:?}", failed.state.outcome);
    };
    assert_eq!(error.name, "Trap");
    assert!(error.message.contains("fuel"), "{}", error.message);
    assert_eq!(kv(&store, "after"), Some(json!(1)));
    assert_eq!(stored(&store, "batch/"), 0);
    let late = block(run.deliver(&store, pending[0].id(), Outcome::success(1), 3))?;
    assert_eq!(late.turns, failed.turns);
    // A trap in the first turn ends the run too.
    let store = MemoryStore::default();
    let failed = start(
        &store,
        "async function main(){while(true){}}",
        Value::Null,
        1,
    )?;
    assert_eq!(failed.view().state.status, Status::Failed);
    Ok(())
}

#[test]
fn the_stack_limit_and_unicode_survive_a_restore() -> Result<()> {
    let source = r#"function overflow(){
        function recurse(){return recurse()+1;}
        try{recurse();return false;}catch(e){return /stack/i.test(e.message);}
    }
    async function main(){
        const before=overflow();
        const text='nul:\u0000 lone:\ud800 snow:雪';
        await durable.call('observe');
        return {before,after:overflow(),nul:text.charCodeAt(4),surrogate:text.charCodeAt(11),snow:text.endsWith('雪')};
    }"#;
    let store = MemoryStore::default();
    start(&store, source, Value::Null, 1000)?;
    let mut run = start(&store, source, Value::Null, 1000)?;
    let result = value(&finish(&mut run, &store)?);
    // JSON cannot carry lone surrogates into Rust strings: test them as code
    // units inside the guest instead.
    assert_eq!(
        result,
        json!({"before":true,"after":true,"nul":0,"surrogate":0xd800,"snow":true})
    );
    Ok(())
}

#[test]
fn pages_back_to_golden_leave_the_commit() -> Result<()> {
    let source = "const bytes=new Uint8Array(128*1024).fill(5);async function main(){await durable.call('observe');bytes.fill(0);await durable.call('observe');const wasZero=bytes[65536]===0;bytes.fill(5);await durable.call('observe');return [wasZero,bytes[65536]];}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let a = run.view();
    let b = step(&mut run, &store)?;
    assert!(b.live_pages > a.live_pages + 20);
    let c = step(&mut run, &store)?;
    assert!(c.live_pages + 20 < b.live_pages);
    assert_eq!(value(&finish(&mut run, &store)?), json!([true, 5]));
    Ok(())
}

/// More local requests than one turn's fuel allows: the turn stops at a safe
/// boundary once half the fuel is spent, and a wakeup goes on in the next.
#[test]
fn long_local_loops_go_on_in_the_next_turn() -> Result<()> {
    let source = "async function main(_,ctx){for(let i=0;i<2000;i++){await ctx.kv.set('k',i);}return await ctx.kv.get('k');}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let mut turns = 1;
    while run.view().state.outcome.is_none() {
        let pending = run.view().state.pending;
        let [Operation::Resume { .. }] = &pending[..] else {
            panic!("Expected one wakeup: {pending:?}");
        };
        step(&mut run, &store)?;
        turns += 1;
    }
    assert_eq!(value(&run.view()), json!(1999));
    assert!(turns > 2, "{turns} turns");
    Ok(())
}

/// Restoring reads every batch the commit references. Past a limit, a turn
/// writes every live page into its own batch, so restoring stays bounded.
/// Only the commit and the batches it references are kept.
#[test]
fn a_run_keeps_a_bounded_number_of_batches() -> Result<()> {
    let source = "const log=[];async function main(_,ctx){for(let i=0;i<60;i++){log.push('x'.repeat(5000)+i);await durable.call('tick');}return log.length+':'+log[59].slice(-2);}";
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let mut most = 0;
    while run.view().state.outcome.is_none() {
        step(&mut run, &store)?;
        let commit: Value = serde_json::from_slice(&store.values()["commit"])?;
        let batches: BTreeSet<u64> = commit["pages"]
            .as_object()
            .unwrap()
            .values()
            .map(|reference| reference["b"].as_u64().unwrap())
            .collect();
        most = most.max(batches.len());
        assert_eq!(stored(&store, "batch/"), batches.len());
    }
    assert_eq!(value(&run.view()), json!("60:59"));
    assert!((2..=16).contains(&most), "{most} batches");
    // A settled run keeps its outcome, not its image.
    assert_eq!(stored(&store, "batch/"), 0);
    Ok(())
}

/// A scope's KV: a turn's reads see its own writes, the scope's runs share
/// it, and a new run replaces the scope's last one, collecting its batches.
#[test]
fn a_scope_shares_its_kv_between_runs() -> Result<()> {
    let first = r#"async function main(_,ctx){
      const v={n:1};const set=ctx.kv.set('x',v);v.n=99;
      await set;const seen=await ctx.kv.get('x');
      await ctx.kv.set('temporary',true);await ctx.kv.delete('temporary');
      await durable.call('forever');
    }"#;
    let store = MemoryStore::default();
    let run = start(&store, first, Value::Null, 1)?;
    assert_eq!(run.view().state.status, Status::Waiting);
    assert_eq!(kv(&store, "x"), Some(json!({"n":1})));
    assert_eq!(kv(&store, "temporary"), None);
    assert!(stored(&store, "batch/") > 0);
    // Its attempt is gone (say, cancelled): another run takes the scope.
    let second = "async function main(_,ctx){return await ctx.kv.get('x');}";
    let programs = Arc::new(
        Programs::new(runtime())
            .compile("v1", first)?
            .compile("v2", second)?,
    );
    let next = block(Run::start(&programs, &store, "next", "v2", Value::Null, 2))?;
    assert_eq!(value(&next.view()), json!({"n":1}));
    assert_eq!(stored(&store, "batch/"), 0);
    Ok(())
}

#[test]
fn delivering_one_request_leaves_the_others_pending() -> Result<()> {
    let source = r#"async function main(_,ctx){return await Promise.all([
        ctx.rpc.call({service:'Echo',handler:'echo'},1),
        ctx.rpc.call({service:'Echo',handler:'echo'},2)]);}"#;
    let store = MemoryStore::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(run.view().state.pending.len(), 2);
    let waiting = block(run.deliver(&store, "r/1", Outcome::success(2), 2))?;
    assert_eq!(waiting.state.pending.len(), 1);
    assert_eq!(waiting.state.pending[0].id(), "r/0");
    let done = block(run.deliver(&store, "r/0", Outcome::success(1), 3))?;
    assert_eq!(value(&done), json!([1, 2]));
    Ok(())
}

#[test]
fn invalid_requests_are_refused_in_the_guest() -> Result<()> {
    let source = r#"async function main(_,ctx){
        let reads=0;
        const value={get text(){return ++reads===1 ? 'valid' : '\ud800';}};
        const bad=[()=>ctx.kv.get(7),()=>ctx.kv.set(null,1),()=>ctx.kv.delete({}),
          ()=>ctx.kv.set('x',undefined),()=>ctx.kv.set('x',new Map()),
          ()=>ctx.rpc.call(null),()=>ctx.rpc.call({service:'',handler:'x'}),
          ()=>ctx.rpc.call({service:'x',handler:'x',key:7}),
          ()=>ctx.rpc.call({service:'x',handler:'x',typo:true}),
          ()=>ctx.rpc.call({service:'x',handler:'x'},()=>1),
          ()=>ctx.kv.set('x','\ud800'),()=>ctx.kv.get('\ud800'),
          ()=>ctx.kv.set('x',{['\ud800']:1}),()=>ctx.call('\ud800'),
          ()=>ctx.kv.set('bad',value)];
        return bad.map(f=>{try{f();return false;}catch(e){return e instanceof TypeError;}});
    }"#;
    let run = start(&MemoryStore::default(), source, Value::Null, 1000)?;
    assert_eq!(value(&run.view()), json!(vec![true; 15]));
    assert_eq!(run.view().state.next_operation, 0);
    // A timer past the host clock's range.
    let source = "async function main(_,ctx){try{await ctx.sleep(1);return false;}catch(e){return e instanceof RangeError;}}";
    let run = start(
        &MemoryStore::default(),
        source,
        Value::Null,
        u64::MAX / 1_000_000,
    )?;
    assert_eq!(value(&run.view()), true);
    Ok(())
}

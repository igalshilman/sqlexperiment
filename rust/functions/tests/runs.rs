//! Runs, turn by turn, over a store in memory: what survives a wait, what a
//! turn commits, and what a new attempt restores.
use anyhow::Result;
use common::*;
use functions::{Limits, Operation, Outcome, Program, Programs, Run, Runtime, Status};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

mod common;

const PROGRAM: &str = r#"export async function main() {
  const state={n:40}; state.self=state;
  const map=new Map([['state',state]]);
  state.n+=await durable.call('increment',{by:2});
  await durable.call('observe');
  return {n:map.get('state').n,identity:state.self===state};
}"#;

#[test]
fn a_new_attempt_restores_closures_cycles_and_maps() -> Result<()> {
    let store = Store::default();
    let mut run = start(&store, PROGRAM, Value::Null, 1000)?;
    assert_eq!(run.view().state.status, Status::Waiting);
    let first = run.view().state.pending[0].id().to_owned();
    block(run.deliver(&first, Outcome::success(2), 1001))?;
    // Another attempt: the VM is restored from the commit.
    let mut run = start(&store, PROGRAM, Value::Null, 1002)?;
    let second = run.view().state.pending[0].id().to_owned();
    let done = block(run.deliver(&second, Outcome::success(Value::Null), 1002))?;
    assert_eq!(value(&done), json!({"n":42,"identity":true}));
    // A request delivered before changes nothing; an unknown one is refused.
    let again = block(run.deliver(&first, Outcome::success(999), 1003))?;
    assert_eq!(again.turns, done.turns);
    assert!(block(run.deliver("r/999", Outcome::success(1), 1004)).is_err());
    Ok(())
}

/// `ctx.key` is the scope the run started in, and stays it on another attempt.
#[test]
fn a_run_knows_its_scope() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      const before=ctx.key;await ctx.sleep(1);return [before,ctx.key];}"#;
    let store = Store::default();
    start(&store, source, Value::Null, 1)?;
    let mut run = start(&store, source, Value::Null, 2)?;
    assert_eq!(value(&finish(&mut run)?), json!(["s", "s"]));
    Ok(())
}

/// A live VM, kept from turn to turn, and one restored at every turn end
/// alike, down to the bytes they store.
#[test]
fn a_live_run_and_a_restored_one_end_alike() -> Result<()> {
    let source = r#"export async function main(_,ctx){
      const log=[];
      for(let i=0;i<5;i++){log.push(await ctx.rpc.call({service:'S',handler:'h'},i));await ctx.kv.set('i',i);}
      await ctx.sleep(10);
      return log.map(x=>x.input);
    }"#;
    let live = Store::default();
    let mut run = start(&live, source, Value::Null, 1)?;
    let expected = finish(&mut run)?;
    let restored = Store::default();
    start(&restored, source, Value::Null, 1)?;
    loop {
        let mut run = start(&restored, source, Value::Null, 1)?;
        if run.view().state.outcome.is_some() {
            assert_eq!(run.view().state, expected.state);
            break;
        }
        step(&mut run)?;
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
    let run = |catalogs: [&Arc<Programs>; 3]| -> Result<Store> {
        let store = Store::default();
        for catalog in catalogs {
            let mut run = block(Run::start(
                catalog,
                &store.scope(),
                "s",
                "r",
                "v1",
                "main",
                Value::Null,
                1,
            ))?;
            if run.view().state.outcome.is_none() {
                step(&mut run)?;
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
    let source = "export async function main(){await durable.call('observe');return 1}";
    let store = Store::default();
    start(&store, source, Value::Null, 1)?;
    let changed = "export async function main(){await durable.call('observe');return 2}";
    let error = start(&store, changed, Value::Null, 1).err().unwrap();
    assert!(
        format!("{error:#}").contains("another version of program v1"),
        "{error:#}"
    );
    let renamed = Arc::new(Programs::new(runtime()).compile("v2", source)?);
    assert!(
        block(Run::start(
            &renamed,
            &store.scope(),
            "s",
            "r",
            "v1",
            "main",
            Value::Null,
            1
        ))
        .is_err()
    );
    let rebuilt = Store::default();
    let mut commit = store.commit();
    commit["runtime"] = json!(0);
    rebuilt.set_commit(&commit);
    assert!(start(&rebuilt, source, Value::Null, 1).is_err());
    // Its own version still completes it.
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(value(&finish(&mut run)?), json!(1));
    Ok(())
}

#[test]
fn promise_race_all_any_settled_and_errors() -> Result<()> {
    let source = r#"export async function main(){
      const a=durable.call('a'), b=durable.call('b');
      const winner=await Promise.race([a,b]);
      const all=await Promise.all([a,b]);
      const outcomes=await Promise.allSettled([durable.call('fail'),Promise.resolve(8)]);
      const any=await Promise.any([Promise.reject('no'),Promise.resolve(9)]);
      return {winner,all,outcomes:outcomes.map(x=>x.status==='rejected'?{status:x.status,message:x.reason.message}:x),any};
    }"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1000)?;
    let pending = run.view().state.pending;
    block(run.deliver(pending[1].id(), Outcome::success(2), 1001))?;
    let state = block(run.deliver(pending[0].id(), Outcome::success(1), 1002))?.state;
    let failure = Outcome::failure("Oops", "failed");
    let done = block(run.deliver(state.pending[0].id(), failure, 1003))?;
    let result = value(&done);
    assert_eq!(result["winner"], json!(2));
    assert_eq!(result["all"], json!([1, 2]));
    assert_eq!(result["outcomes"][0]["status"], "rejected");
    assert_eq!(result["any"], 9);
    Ok(())
}

#[test]
fn a_run_ends_when_main_settles() -> Result<()> {
    let source = "export async function main(_,ctx){const a=durable.call('a'), b=durable.call('b').catch(()=>ctx.kv.set('late',1));return await Promise.race([a,b]);}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let pending = run.view().state.pending;
    assert_eq!(pending.len(), 2);
    let done = block(run.deliver(pending[0].id(), Outcome::success(7), 2))?;
    // The loser is dropped: nothing waits on it, and its continuation never runs.
    assert_eq!(done.state.status, Status::Completed);
    assert_eq!(value(&done), 7);
    assert!(done.state.pending.is_empty());
    let late = block(run.deliver(pending[1].id(), Outcome::failure("E", "x"), 3))?;
    assert_eq!(late.turns, done.turns);
    assert_eq!(kv(&store, "late"), None);
    Ok(())
}

#[test]
fn logical_time_seeded_random_and_timers() -> Result<()> {
    let source = "export async function main(){ const a=[Date.now(),new Date().getTimezoneOffset(),Math.random()]; await durable.sleep(50); return [a,Date.now(),Math.random()]; }";
    let (a, b) = (Store::default(), Store::default());
    let mut x = start(&a, source, Value::Null, 1000)?;
    let mut y = start(&b, source, Value::Null, 1000)?;
    let timer = x.view().state.pending[0].id().to_owned();
    assert!(block(x.deliver(&timer, Outcome::success(Value::Null), 1049)).is_err());
    let x = block(x.deliver(&timer, Outcome::success(Value::Null), 1050))?;
    let y = block(y.deliver(&timer, Outcome::success(Value::Null), 1050))?;
    assert_eq!(x.state.outcome, y.state.outcome);
    let result = value(&x);
    assert_eq!(result[0][0], 1000);
    assert_eq!(result[0][1], 0);
    assert_eq!(result[1], 1050);
    Ok(())
}

#[test]
fn memory_growth_survives_a_restore() -> Result<()> {
    let source = "export async function main(){await durable.call('observe'); const b=new Uint8Array(3*1024*1024); b[0]=7;b[b.length-1]=9;await durable.call('observe');b[0]=0;await durable.call('observe');return [b[0],b[b.length-1],b[4096]];}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let initial = run.view().memory_bytes;
    let grown = step(&mut run)?;
    // The 3 MiB buffer is mostly new memory: some of it may fit in the
    // heap's free space.
    assert!(grown.memory_bytes > initial + 2 * 1024 * 1024);
    assert!(grown.batch_bytes < 100_000);
    step(&mut run)?;
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(value(&finish(&mut run)?), json!([0, 9, 0]));
    Ok(())
}

#[test]
fn programs_are_checked_and_failures_end_runs() -> Result<()> {
    for source in [
        "durable.call('forbidden');export async function main(){}",
        "export async function main( {",
        "async function notMain(){}",
    ] {
        assert!(Programs::new(runtime()).compile("bad", source).is_err());
    }
    let main = "export async function main(){}";
    assert!(Program::compile(&runtime(), "not a name", main).is_err());
    assert!(
        Programs::new(runtime())
            .compile("v1", main)?
            .compile("v1", main)
            .is_err()
    );
    for source in [
        "export async function main(){throw new Error('boom')}",
        "export async function main(){return ()=>1}",
        "export async function main(){await new Promise(()=>{})}",
    ] {
        let run = start(&Store::default(), source, Value::Null, 1)?;
        assert_eq!(run.view().state.status, Status::Failed, "{source}");
    }
    let run = start(
        &Store::default(),
        "export async function main(){}",
        Value::Null,
        1,
    )?;
    assert_eq!(value(&run.view()), Value::Null);
    Ok(())
}

/// A synchronous job longer than a turn allows traps, and would trap again:
/// the run ends instead. The KV writes before the trap stand, as for a thrown
/// error; its requests still outstanding are dropped.
#[test]
fn a_trap_ends_the_run() -> Result<()> {
    let source = "export async function main(_,ctx){await ctx.kv.set('before',1);ctx.rpc.call({service:'S',handler:'h'});await durable.call('observe');await ctx.kv.set('after',1);while(true){}}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let pending = run.view().state.pending;
    assert_eq!(pending.len(), 2);
    let failed = block(run.deliver(pending[1].id(), Outcome::success(Value::Null), 2))?;
    assert_eq!(failed.state.status, Status::Failed);
    assert!(failed.state.pending.is_empty());
    let Some(Outcome::Failure { error, .. }) = &failed.state.outcome else {
        panic!("Expected a failure: {:?}", failed.state.outcome);
    };
    assert_eq!(error.name, "Trap");
    assert!(error.message.contains("Out of time"), "{}", error.message);
    assert_eq!(kv(&store, "after"), Some(json!(1)));
    assert_eq!(store.batches(), 0);
    let late = block(run.deliver(pending[0].id(), Outcome::success(1), 3))?;
    assert_eq!(late.turns, failed.turns);
    // A trap in the first turn ends the run too.
    let store = Store::default();
    let failed = start(
        &store,
        "export async function main(){while(true){}}",
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
    export async function main(){
        const before=overflow();
        const text='nul:\u0000 lone:\ud800 snow:雪';
        await durable.call('observe');
        return {before,after:overflow(),nul:text.charCodeAt(4),surrogate:text.charCodeAt(11),snow:text.endsWith('雪')};
    }"#;
    let store = Store::default();
    start(&store, source, Value::Null, 1000)?;
    let mut run = start(&store, source, Value::Null, 1000)?;
    let result = value(&finish(&mut run)?);
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
    let source = "const bytes=new Uint8Array(128*1024).fill(5);export async function main(){await durable.call('observe');bytes.fill(0);await durable.call('observe');const wasZero=bytes[65536]===0;bytes.fill(5);await durable.call('observe');return [wasZero,bytes[65536]];}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let a = run.view();
    let b = step(&mut run)?;
    assert!(b.live_pages > a.live_pages + 20);
    let c = step(&mut run)?;
    assert!(c.live_pages + 20 < b.live_pages);
    assert_eq!(value(&finish(&mut run)?), json!([true, 5]));
    Ok(())
}

/// What a turn allocates and frees is not saved: capture zeroes free heap
/// chunks, so the pages the garbage was written to end as they were.
#[test]
fn garbage_a_turn_frees_is_not_saved() -> Result<()> {
    let source = "export async function main(){let n=0;for(let i=0;i<5;i++){n+=JSON.stringify(Array.from({length:4000},(_,j)=>j*i+Math.random())).length;await durable.call('tick');}return n>0;}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    step(&mut run)?;
    for _ in 0..3 {
        let turn = step(&mut run)?;
        // ~100 KiB of strings and arrays went through the heap: without
        // zeroing, ~60 pages changed and the batch took ~70 KiB. With it, a
        // few KiB: how much depends on when the GC runs, which moves with the
        // size of the bridge's heap.
        assert!(turn.batch_bytes < 8192, "{} bytes", turn.batch_bytes);
    }
    assert_eq!(value(&finish(&mut run)?), json!(true));
    Ok(())
}

/// More local requests than one turn's time allows: the turn stops at a safe
/// boundary past the checkpoint limit, and a wakeup goes on in the next.
#[test]
fn long_local_loops_go_on_in_the_next_turn() -> Result<()> {
    let source = "export async function main(_,ctx){for(let i=0;i<2000;i++){await ctx.kv.set('k',i);}return await ctx.kv.get('k');}";
    let limits = Limits {
        checkpoint: Duration::from_millis(5),
        ..Limits::default()
    };
    let programs =
        Arc::new(Programs::new(Arc::new(Runtime::new()?.limited(limits))).compile("v1", source)?);
    let store = Store::default();
    let mut run = block(Run::start(
        &programs,
        &store.scope(),
        "s",
        "r",
        "v1",
        "main",
        Value::Null,
        1,
    ))?;
    let mut turns = 1;
    while run.view().state.outcome.is_none() {
        let pending = run.view().state.pending;
        let [Operation::Resume { .. }] = &pending[..] else {
            panic!("Expected one wakeup: {pending:?}");
        };
        step(&mut run)?;
        turns += 1;
    }
    assert_eq!(value(&run.view()), json!(1999));
    assert!(turns > 2, "{turns} turns");
    Ok(())
}

/// Restoring applies every batch of the commit's chain. Past a limit, a turn
/// writes every live page into a new first batch, so restoring stays bounded.
#[test]
fn a_run_keeps_a_bounded_number_of_batches() -> Result<()> {
    let source = "const log=[];export async function main(_,ctx){for(let i=0;i<60;i++){log.push('x'.repeat(5000)+i);await durable.call('tick');}return log.length+':'+log[59].slice(-2);}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let mut most = 0;
    while run.view().state.outcome.is_none() {
        step(&mut run)?;
        let batches = store.commit()["batches"].as_u64().unwrap() as usize;
        most = most.max(batches);
        assert_eq!(store.batches(), batches);
    }
    assert_eq!(value(&run.view()), json!("60:59"));
    assert!((2..=32).contains(&most), "{most} batches");
    // A settled run keeps its outcome, not its image.
    assert_eq!(store.batches(), 0);
    Ok(())
}

/// A big image and small turns: the chain grows instead, and the image is
/// written once, not again every few turns. A restore reads the whole chain.
#[test]
fn small_turns_do_not_rewrite_a_big_image() -> Result<()> {
    let source = "export async function main(){const words=new Uint32Array(8*1024);for(let i=0;i<words.length;i++)words[i]=Math.random()*2**32;for(let i=0;i<40;i++){words[i]=i;await durable.call('tick');}return words[39];}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    for _ in 0..39 {
        step(&mut run)?;
    }
    assert_eq!(store.batches(), 40);
    // The first batch holds 32 KiB of noise; the others, a word each (~112
    // KiB in all). A new first batch every 16 turns took ~155 KiB.
    let written = store.bytes_written();
    assert!(written < 120 * 1024, "{written} bytes");
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(value(&finish(&mut run)?), json!(39));
    Ok(())
}

/// Outcomes that arrive together go in one turn, in order, and commit once.
#[test]
fn outcomes_delivered_together_take_one_turn() -> Result<()> {
    let source = "export async function main(_,ctx){const order=[];await Promise.all([1,2,3].map(async n=>{await ctx.rpc.call({service:'S',handler:'h'},n);order.push(n);}));return order;}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let started = run.view();
    let mut outcomes: Vec<_> = started
        .state
        .pending
        .iter()
        .map(|operation| (operation.id().to_owned(), answer(&mut run, operation)))
        .collect();
    outcomes.reverse();
    let duplicate = vec![outcomes[0].clone(), outcomes[0].clone()];
    assert!(block(run.deliver_all(duplicate, 2)).is_err());
    let mut run = start(&store, source, Value::Null, 2)?;
    let done = block(run.deliver_all(outcomes.clone(), 2))?;
    assert_eq!(done.turns, started.turns + 1);
    assert_eq!(value(&done), json!([3, 2, 1]));
    // Delivered before: nothing to do.
    assert_eq!(block(run.deliver_all(outcomes, 3))?.turns, done.turns);
    Ok(())
}

/// A call's input stays in the heap until the call is answered: the commit
/// keeps the call's ID, not a copy, and the host reads the input from the
/// heap, the same after a restore. The guest's own idempotency key goes with
/// the call; without one, the call's key is its ID.
#[test]
fn a_call_input_stays_in_the_heap_until_answered() -> Result<()> {
    let source = "const prompt=()=>JSON.stringify(Array.from({length:8000},(_,i)=>i*7919%10007));export async function main(_,ctx){const r=await ctx.rpc.call({service:'S',handler:'h'},{prompt:prompt()},{idempotencyKey:'mine'});const c=await durable.call('model',{n:1n}).catch(e=>e.name+': '+e.message);return [r.input.prompt.length,c];}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let issued = run.view();
    // ~39 KiB of JSON, kept in the heap, compressed; none in the commit.
    assert!(
        (8 * 1024..32 * 1024).contains(&issued.batch_bytes),
        "{} bytes",
        issued.batch_bytes
    );
    assert!(issued.commit_bytes < 2048, "{} bytes", issued.commit_bytes);
    let call = &issued.state.pending[0];
    assert_eq!(call.idempotency_key(), "mine");
    let input = block(run.input(call.id()))?.unwrap();
    // Another attempt restores the heap to read it.
    let mut run = start(&store, source, Value::Null, 2)?;
    assert_eq!(block(run.input(call.id()))?.unwrap(), input);
    assert!(input["prompt"].as_str().unwrap().len() > 32_000);
    step(&mut run)?;
    // An input JSON cannot hold fails its call, as it is read.
    let model = run.view().state.pending[0].clone();
    assert_eq!(model.idempotency_key(), model.id());
    let error = block(run.input(model.id()))?.unwrap_err();
    assert_eq!(error.name, "TypeError");
    let failed = Outcome::failure(error.name, error.message);
    let done = value(&block(run.deliver(model.id(), failed, 3))?);
    assert!(done[0].as_u64() > Some(32_000));
    assert!(
        done[1].as_str().unwrap().starts_with("TypeError: "),
        "{done}"
    );
    Ok(())
}

/// A request that loses a race may fail later, unobserved: the run goes on.
#[test]
fn a_losing_request_may_fail_later() -> Result<()> {
    let source = "export async function main(_,ctx){const call=ctx.rpc.call({service:'S',handler:'h'},1);const won=await Promise.race([call,ctx.sleep(1).then(()=>'timer')]);await ctx.sleep(10);return won;}";
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    let pending = run.view().state.pending;
    let [
        Operation::Rpc { id: call, .. },
        Operation::Sleep {
            id: timer,
            deadline,
        },
    ] = &pending[..]
    else {
        panic!("Expected a call and a timer: {pending:?}");
    };
    block(run.deliver(timer, Outcome::success(Value::Null), *deadline))?;
    let failed = Outcome::failure("Error", "too late");
    block(run.deliver(call, failed, deadline + 1))?;
    assert_eq!(value(&finish(&mut run)?), json!("timer"));
    Ok(())
}

/// A scope's KV: a turn's reads see its own writes, the scope's runs share
/// it, and a new run replaces the scope's last one, collecting its batches.
#[test]
fn a_scope_shares_its_kv_between_runs() -> Result<()> {
    let first = r#"export async function main(_,ctx){
      const v={n:1};const set=ctx.kv.set('x',v);v.n=99;
      await set;const seen=await ctx.kv.get('x');
      await ctx.kv.set('temporary',true);await ctx.kv.delete('temporary');
      await durable.call('forever');
    }"#;
    let store = Store::default();
    let run = start(&store, first, Value::Null, 1)?;
    assert_eq!(run.view().state.status, Status::Waiting);
    assert_eq!(kv(&store, "x"), Some(json!({"n":1})));
    assert_eq!(kv(&store, "temporary"), None);
    assert!(store.batches() > 0);
    // Its attempt is gone (say, cancelled): another run takes the scope.
    let second = "export async function main(_,ctx){return await ctx.kv.get('x');}";
    let programs = Arc::new(
        Programs::new(runtime())
            .compile("v1", first)?
            .compile("v2", second)?,
    );
    let next = block(Run::start(
        &programs,
        &store.scope(),
        "s",
        "next",
        "v2",
        "main",
        Value::Null,
        2,
    ))?;
    assert_eq!(value(&next.view()), json!({"n":1}));
    assert_eq!(store.batches(), 0);
    Ok(())
}

#[test]
fn delivering_one_request_leaves_the_others_pending() -> Result<()> {
    let source = r#"export async function main(_,ctx){return await Promise.all([
        ctx.rpc.call({service:'Echo',handler:'echo'},1),
        ctx.rpc.call({service:'Echo',handler:'echo'},2)]);}"#;
    let store = Store::default();
    let mut run = start(&store, source, Value::Null, 1)?;
    assert_eq!(run.view().state.pending.len(), 2);
    let waiting = block(run.deliver("r/1", Outcome::success(2), 2))?;
    assert_eq!(waiting.state.pending.len(), 1);
    assert_eq!(waiting.state.pending[0].id(), "r/0");
    let done = block(run.deliver("r/0", Outcome::success(1), 3))?;
    assert_eq!(value(&done), json!([1, 2]));
    Ok(())
}

#[test]
fn invalid_requests_are_refused_in_the_guest() -> Result<()> {
    let source = r#"export async function main(_,ctx){
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
          ()=>ctx.kv.set('bad',value),()=>ctx.call('x',null,{idempotencyKey:''}),
          ()=>ctx.call('x',null,{key:'typo'}),()=>ctx.rpc.call({service:'x',handler:'x'},1,{idempotencyKey:7})];
        return bad.map(f=>{try{f();return false;}catch(e){return e instanceof TypeError;}});
    }"#;
    let run = start(&Store::default(), source, Value::Null, 1000)?;
    assert_eq!(value(&run.view()), json!(vec![true; 18]));
    assert_eq!(run.view().state.next_operation, 0);
    // A timer past the host clock's range.
    let source = "export async function main(_,ctx){try{await ctx.sleep(1);return false;}catch(e){return e instanceof RangeError;}}";
    let run = start(&Store::default(), source, Value::Null, u64::MAX / 1_000_000)?;
    assert_eq!(value(&run.view()), true);
    Ok(())
}

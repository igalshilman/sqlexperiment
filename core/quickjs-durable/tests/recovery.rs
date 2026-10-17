use anyhow::Result;
use quickjs_durable::{Engine, MemoryTransaction, Outcome, Runtime, Status};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

fn runtime() -> Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| Arc::new(Runtime::new().expect("compile QuickJS")))
        .clone()
}
fn engine(source: &str) -> Engine<MemoryTransaction> {
    let mut e = Engine::new(MemoryTransaction::default(), runtime());
    e.register("v1", source).expect("register");
    e
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
    let mut e = Engine::new(e.into_storage()?, runtime());
    let step = futures_executor::block_on(e.complete(
        "run",
        start.state.pending[0].id(),
        Outcome::success(2),
        1001,
    ))?;
    let mut e = Engine::new(e.into_storage()?, runtime());
    let done = futures_executor::block_on(e.complete(
        "run",
        step.state.pending[0].id(),
        Outcome::success(Value::Null),
        1002,
    ))?;
    assert_eq!(value(&done.state.outcome), json!({"n":42,"identity":true}));
    assert_eq!(e.recover("run")?.checkpoint_id, done.checkpoint_id);
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
    let mut e = engine(source);
    e.register("v1", source)?;
    let registered = e.into_storage()?;
    let mut invalid = Engine::new(registered.clone(), runtime());
    assert!(
        invalid
            .register("v1", "async function main(){return 0}")
            .is_err()
    );
    assert!(invalid.into_storage().is_err());
    let mut e = Engine::new(registered, runtime());
    let a = futures_executor::block_on(e.start("a", "v1", json!(2), 1000))?;
    let b = futures_executor::block_on(e.start("b", "v1", json!(7), 1000))?;
    e.register("v2", "async function main(){return 99}")?;
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
    assert_eq!(
        e.storage()
            .values
            .keys()
            .filter(|k| k.starts_with("application/"))
            .count(),
        2
    );
    let committed = e.into_storage()?;
    for (version, input) in [("v2", json!(2)), ("v1", json!(3))] {
        let mut invalid = Engine::new(committed.clone(), runtime());
        assert!(futures_executor::block_on(invalid.start("a", version, input, 1003)).is_err());
        assert!(invalid.into_storage().is_err());
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
fn losing_race_is_drained_before_terminal() -> Result<()> {
    let mut e = engine(
        "async function main(){return await Promise.race([durable.call('a'),durable.call('b')]);}",
    );
    let start = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let waiting = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[0].id(),
        Outcome::success(7),
        2,
    ))?;
    assert_eq!(waiting.state.status, Status::Waiting);
    assert_eq!(value(&waiting.state.outcome), 7);
    let done = futures_executor::block_on(e.complete(
        "r",
        start.state.pending[1].id(),
        Outcome::failure("Error", "loser"),
        3,
    ))?;
    assert_eq!(done.state.status, Status::Completed);
    assert_eq!(value(&done.state.outcome), 7);
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
    let mut a = Engine::new(committed.clone(), runtime());
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
    let mut a = Engine::new(committed, runtime());
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
    let mut e = engine(
        "async function main(){await durable.call('observe'); const b=new Uint8Array(3*1024*1024); b[0]=7;b[b.length-1]=9;await durable.call('observe');b[0]=0;await durable.call('observe');return [b[0],b[b.length-1],b[4096]];}",
    );
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
    let mut kv = e.into_storage()?;
    // History IDs remain in metadata, but are not needed to restore this head.
    kv.values
        .retain(|key, _| !key.starts_with("checkpoint/") || key.ends_with(&state.checkpoint_id));
    let mut e = Engine::new(kv, runtime());
    assert_eq!(e.recover("r")?.checkpoint_id, state.checkpoint_id);
    let done = futures_executor::block_on(e.complete(
        "r",
        state.state.pending[0].id(),
        Outcome::success(Value::Null),
        4,
    ))?;
    assert_eq!(value(&done.state.outcome), json!([0, 9, 0]));
    Ok(())
}

#[test]
fn missing_golden_and_incompatible_build_are_rejected() -> Result<()> {
    let mut e = engine(PROGRAM);
    futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let original = e.into_storage()?;
    for mode in ["application", "build"] {
        let mut kv = original.clone();
        if mode == "application" {
            kv.values.retain(|k, _| !k.starts_with("application/"));
        }
        if mode == "build" {
            for (k, v) in &mut kv.values {
                if k.starts_with("application/") {
                    let mut j: Value = serde_json::from_slice(v)?;
                    j["build"] = json!("wrong");
                    *v = serde_json::to_vec(&j)?;
                }
            }
        }
        assert!(
            Engine::new(kv, runtime()).recover("r").is_err(),
            "mode={mode}"
        );
    }
    Ok(())
}

#[test]
fn initialization_and_guest_failure_contract() -> Result<()> {
    for source in [
        "durable.call('forbidden');async function main(){}",
        "async function main( {",
    ] {
        let mut e = Engine::new(MemoryTransaction::default(), runtime());
        assert!(e.register("bad", source).is_err());
        assert!(
            !e.storage()
                .values
                .keys()
                .any(|k| k.starts_with("application/"))
        );
    }
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

#[test]
fn fuel_exhaustion_preserves_committed_head() -> Result<()> {
    let mut e = engine("async function main(){await durable.call('observe');while(true){} }");
    let before = futures_executor::block_on(e.start("r", "v1", Value::Null, 1))?;
    let committed = e.into_storage()?;
    let mut e = Engine::new(committed.clone(), runtime());
    let error = futures_executor::block_on(e.complete(
        "r",
        before.state.pending[0].id(),
        Outcome::success(Value::Null),
        2,
    ))
    .unwrap_err();
    assert!(format!("{error:#}").contains("fuel"), "{error:#}");
    let head = e
        .storage()
        .values
        .iter()
        .find(|(k, _)| k.ends_with("/head"))
        .unwrap()
        .1;
    assert_eq!(head, before.checkpoint_id.as_bytes());
    assert!(e.recover("r").is_err());
    assert!(e.into_storage().is_err());
    assert_eq!(
        Engine::new(committed, runtime())
            .recover("r")?
            .checkpoint_id,
        before.checkpoint_id
    ); // no event replay
    Ok(())
}

#[test]
fn stack_limit_and_unicode_survive_cold_restore() -> Result<()> {
    let mut e = engine(
        r#"function overflow(){
        function recurse(){return recurse()+1;}
        try{recurse();return false;}catch(e){return /stack/i.test(e.message);}
    }
    async function main(){
        const before=overflow();
        const text='nul:\u0000 lone:\ud800 snow:\u96ea';
        await durable.call('observe');
        return {before,after:overflow(),nul:text.charCodeAt(4),surrogate:text.charCodeAt(11),snow:text.endsWith('雪')};
    }"#,
    );
    let first = futures_executor::block_on(e.start("r", "v1", Value::Null, 1000))?;
    let mut e = Engine::new(e.into_storage()?, runtime());
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

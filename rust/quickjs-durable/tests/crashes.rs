//! A crash at any point: the store keeps any prefix of a run's writes (Restate
//! loses a suffix of unconfirmed ones), and another attempt starts over it.
//! Every such attempt must end exactly as the run without a crash: the same
//! outcome, the same KV, no batch left behind.
use anyhow::Result;
use common::*;
use quickjs_durable::MemoryStore;
use serde_json::{Value, json};

mod common;

const FLOW: &str = r#"async function main(input, ctx){
    const before=(await ctx.kv.get('n')) ?? 0;
    await ctx.kv.set('n', before+input);
    const echoed=await ctx.rpc.call({service:'Echo',handler:'echo'},{n:before+input});
    await ctx.kv.set('echoed',echoed.input.n);
    const big=new Uint8Array(512*1024).fill(1);
    await ctx.sleep(5);
    big[1000]=2;
    await ctx.kv.delete('n');
    await durable.call('observe');
    return {sum:big[0]+big[1000], id:echoed.id};
}"#;

#[test]
fn an_attempt_after_a_crash_at_any_write_ends_as_without_one() -> Result<()> {
    let reference = MemoryStore::default();
    let mut run = start(&reference, FLOW, json!(5), 1)?;
    let expected = finish(&mut run, &reference)?;
    assert_eq!(value(&expected), json!({"sum":3,"id":"r/2"}));
    let expected_kv = (kv(&reference, "n"), kv(&reference, "echoed"));
    assert_eq!(expected_kv, (None, Some(json!(5))));
    assert_eq!(stored(&reference, "batch/"), 0);
    for kept in 0..=reference.writes() {
        let store = reference.crashed(kept);
        let mut run = start(&store, FLOW, json!(5), 1)?;
        let done = finish(&mut run, &store)?;
        assert_eq!(done.state, expected.state, "after {kept} writes");
        assert_eq!(
            (kv(&store, "n"), kv(&store, "echoed")),
            expected_kv,
            "after {kept} writes"
        );
        assert_eq!(stored(&store, "batch/"), 0, "after {kept} writes");
    }
    Ok(())
}

/// A crash, then another during the attempt that recovers from it, at each
/// of its own writes: a third attempt still ends as without a crash.
#[test]
fn a_crash_while_recovering_from_one_changes_nothing() -> Result<()> {
    let reference = MemoryStore::default();
    let mut run = start(&reference, FLOW, json!(5), 1)?;
    let expected = finish(&mut run, &reference)?;
    let mut attempts = 0;
    for kept in 0..=reference.writes() {
        let recovering = reference.crashed(kept);
        let mut run = start(&recovering, FLOW, json!(5), 1)?;
        finish(&mut run, &recovering)?;
        for again in kept..=recovering.writes() {
            let store = recovering.crashed(again);
            let mut run = start(&store, FLOW, json!(5), 1)?;
            let done = finish(&mut run, &store)?;
            assert_eq!(done.state, expected.state, "after {kept}, then {again}");
            assert_eq!(kv(&store, "echoed"), Some(json!(5)));
            assert_eq!(kv(&store, "n"), None::<Value>);
            assert_eq!(stored(&store, "batch/"), 0);
            attempts += 1;
        }
    }
    assert!(attempts > reference.writes() * 2, "{attempts} attempts");
    Ok(())
}

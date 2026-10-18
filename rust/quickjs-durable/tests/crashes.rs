//! A crash at any point: a write to the store fails (it landed or not), the
//! attempt ends, and what was not acknowledged may be lost, in any order.
//! Another attempt starts over what is left, through the process's owner
//! cache or as a new process. Every such attempt must end exactly as the run
//! without a crash: the same outcome, the same KV, no batch left behind.
use anyhow::Result;
use common::*;
use quickjs_durable::{Run, Scope, View};
use serde_json::{Value, json};
use sqlite_kv::{
    NamespaceCache,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation, fresh_namespace},
};
use std::sync::Arc;

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

/// A store whose `nth` write (put or delete) fails, read back through `memory`.
struct Faulty {
    memory: Store,
    faults: Arc<FaultKv>,
}

impl Faulty {
    fn new() -> Self {
        let memory = MemoryKv::default();
        Self {
            memory: Store {
                kv: Arc::new(memory.clone()),
            },
            faults: Arc::new(FaultKv::new(memory, 0)),
        }
    }

    fn scope(&self, cache: &Option<NamespaceCache>) -> Scope {
        let scope = Scope::new(self.faults.clone());
        match cache {
            Some(cache) => scope.cached(cache.clone()),
            None => scope,
        }
    }

    fn arm(&self, operation: Operation, effect: Effect, nth: usize) {
        drop(self.faults.arm(Failpoint {
            operation,
            key_prefix: "v14/".into(),
            nth,
            effect,
            cancel: false,
        }));
    }

    /// Whether the armed failure happened.
    fn faulted(&self) -> bool {
        self.faults.events().iter().any(|event| event.faulted)
    }

    /// Stop failing; lose what was not acknowledged.
    fn crash(&self) {
        self.faults.disarm();
        self.faults.inner.crash(&[]);
    }
}

/// The run to its end in `scope`: started, or another attempt.
fn attempt(scope: &Scope) -> Result<View> {
    let mut run: Run = start_in(scope, FLOW, json!(5), 1)?;
    finish(&mut run)
}

fn reference() -> Result<View> {
    let store = Store::default();
    let expected = attempt(&store.scope())?;
    assert_eq!(value(&expected), json!({"sum":3,"id":"r/2"}));
    assert_eq!(
        (kv(&store, "n"), kv(&store, "echoed")),
        (None, Some(json!(5)))
    );
    assert_eq!(store.batches(), 0);
    Ok(expected)
}

fn assert_ended_as(store: &Store, done: &View, expected: &View, case: &str) {
    assert_eq!(done.state, expected.state, "{case}");
    assert_eq!(kv(store, "n"), None::<Value>, "{case}");
    assert_eq!(kv(store, "echoed"), Some(json!(5)), "{case}");
    assert_eq!(store.batches(), 0, "{case}");
}

#[test]
fn an_attempt_after_a_crash_at_any_write_ends_as_without_one() -> Result<()> {
    let expected = reference()?;
    let mut cases = 0;
    for cached in [false, true] {
        for effect in [Effect::None, Effect::After] {
            for operation in [Operation::Put, Operation::Delete] {
                for nth in 1.. {
                    let store = Faulty::new();
                    let cache = cached.then(fresh_namespace);
                    store.arm(operation, effect, nth);
                    let failed = attempt(&store.scope(&cache)).is_err();
                    if !store.faulted() {
                        assert!(!failed, "a run fails without a fault");
                        break;
                    }
                    store.crash();
                    let case = format!("{effect:?} {operation:?} #{nth} cached={cached}");
                    let done = attempt(&store.scope(&cache))?;
                    assert_ended_as(&store.memory, &done, &expected, &case);
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 20, "{cases} cases");
    Ok(())
}

/// A crash, then another during the attempt that recovers from it, at each
/// of its own writes: a third attempt still ends as without a crash.
#[test]
fn a_crash_while_recovering_from_one_changes_nothing() -> Result<()> {
    let expected = reference()?;
    let mut attempts = 0;
    for first in 1.. {
        let probe = Faulty::new();
        probe.arm(Operation::Put, Effect::None, first);
        let _ = attempt(&probe.scope(&None));
        if !probe.faulted() {
            break;
        }
        for second in 1.. {
            let store = Faulty::new();
            store.arm(Operation::Put, Effect::None, first);
            // A failed seal or cleanup leaves the attempt standing.
            let _ = attempt(&store.scope(&None));
            store.crash();
            store.arm(Operation::Put, Effect::After, second);
            let recovered = attempt(&store.scope(&None));
            let faulted = store.faulted();
            store.crash();
            if !faulted {
                assert_ended_as(&store.memory, &recovered?, &expected, "");
                break;
            }
            let done = attempt(&store.scope(&None))?;
            assert_ended_as(
                &store.memory,
                &done,
                &expected,
                &format!("#{first}, then #{second}"),
            );
            attempts += 1;
        }
    }
    assert!(attempts > 10, "{attempts} attempts");
    Ok(())
}

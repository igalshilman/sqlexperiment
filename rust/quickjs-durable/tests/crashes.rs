//! A crash at any point: the attempt ends, and of its writes since the last
//! sync, the store keeps a prefix (as Restate keeps a prefix of an attempt's
//! journal). Another attempt starts over what is left, through the process's
//! owner cache or as a new process. Every such attempt must end exactly as
//! the run without a crash: the same outcome, the same KV, no batch left
//! behind.
use anyhow::Result;
use common::*;
use quickjs_durable::{Language, Run, Scope, View};
use serde_json::{Value, json};
use sqlite_kv::{
    NamespaceCache,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation, fresh_namespace},
};
use std::sync::Arc;

mod common;

const FLOW: &str = r#"export async function main(input, ctx){
    // The heap counts the rows it inserted; the database must agree, whatever
    // a crash kept: a turn's SQL commits with the turn.
    ctx.db.sql`CREATE TABLE IF NOT EXISTS log(n INTEGER)`;
    let inserted=0;
    const insert=()=>ctx.db.sql`INSERT INTO log VALUES(${++inserted})`;
    const before=(await ctx.kv.get('n')) ?? 0;
    insert();
    await ctx.kv.set('n', before+input);
    const echoed=await ctx.rpc.call({service:'Echo',handler:'echo'},{n:before+input});
    insert();
    await ctx.kv.set('echoed',echoed.input.n);
    const big=new Uint8Array(512*1024).fill(1);
    await ctx.sleep(5);
    insert();
    try{ctx.db.transaction(()=>{ctx.db.sql`INSERT INTO log VALUES(${-1})`;throw new Error('undo');});}catch{}
    big[1000]=2;
    await ctx.kv.delete('n');
    await durable.call('observe');
    const [{rows}]=ctx.db.sql`SELECT count(*) AS rows FROM log`;
    return {sum:big[0]+big[1000], id:echoed.id, rows, inserted};
}"#;

/// FLOW, in Python.
const FLOW_PY: &str = r#"
async def main(input, ctx):
    # The heap counts the rows it inserted; the database must agree, whatever
    # a crash kept: a turn's SQL commits with the turn.
    ctx.db.sql("CREATE TABLE IF NOT EXISTS log(n INTEGER)")
    inserted = 0

    def insert():
        nonlocal inserted
        inserted += 1
        ctx.db.sql(t"INSERT INTO log VALUES({inserted})")

    before = await ctx.kv.get("n") or 0
    insert()
    await ctx.kv.set("n", before + input)
    echoed = await ctx.rpc.call("Echo", "echo", {"n": before + input})
    insert()
    await ctx.kv.set("echoed", echoed["input"]["n"])
    big = bytearray(b"" * (512 * 1024))
    await ctx.sleep(0.005)
    insert()
    try:
        with ctx.db.transaction():
            ctx.db.sql("INSERT INTO log VALUES(-1)")
            raise RuntimeError("undo")
    except RuntimeError:
        pass
    big[1000] = 2
    await ctx.kv.delete("n")
    await ctx.call("observe")
    [row] = ctx.db.sql("SELECT count(*) AS rows FROM log")
    return {"sum": big[0] + big[1000], "id": echoed["id"], "rows": row["rows"], "inserted": inserted}
"#;

/// The flow in each language.
const FLOWS: [(Language, &str); 2] = [(Language::JavaScript, FLOW), (Language::Python, FLOW_PY)];

/// A store whose `nth` sync fails, storing nothing, read back through
/// `memory`.
struct Faulty {
    memory: Store,
    faults: Arc<FaultKv>,
}

impl Faulty {
    fn new() -> Self {
        Self::over(MemoryKv::default())
    }

    fn over(memory: MemoryKv) -> Self {
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

    fn arm(&self, nth: usize) {
        drop(self.faults.arm(Failpoint {
            operation: Operation::Sync,
            key_prefix: String::new(),
            nth,
            effect: Effect::None,
            cancel: false,
        }));
    }

    /// Whether the armed failure happened.
    fn faulted(&self) -> bool {
        self.faults.events().iter().any(|event| event.faulted)
    }

    /// The writes since the last sync: what a crash may lose.
    fn unsynced(&self) -> usize {
        self.faults.inner.unsynced().len()
    }

    /// The store as a crash leaves it, keeping the first `kept` writes since
    /// the last sync.
    fn crashed(&self, kept: usize) -> Faulty {
        self.faults.disarm();
        let memory = self.faults.inner.fork();
        memory.crash_keeping(kept);
        Self::over(memory)
    }
}

/// The run of `flow` to its end in `scope`: started, or another attempt.
fn attempt(flow: (Language, &str), scope: &Scope) -> Result<View> {
    let mut run: Run = block(Run::start(
        &catalog(flow.0, flow.1),
        scope,
        "s",
        "r",
        "v1",
        "main",
        json!(5),
        1,
    ))?;
    finish(&mut run)
}

fn reference(flow: (Language, &str)) -> Result<View> {
    let store = Store::default();
    let expected = attempt(flow, &store.scope())?;
    assert_eq!(
        value(&expected),
        json!({"sum":3,"id":"r/2","rows":3,"inserted":3})
    );
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
    for flow in FLOWS {
        crash_at_any_write(flow)?;
    }
    Ok(())
}

fn crash_at_any_write(flow: (Language, &str)) -> Result<()> {
    let expected = reference(flow)?;
    let mut cases = 0;
    for cached in [false, true] {
        for nth in 1.. {
            let store = Faulty::new();
            let cache = cached.then(fresh_namespace);
            store.arm(nth);
            let failed = attempt(flow, &store.scope(&cache)).is_err();
            if !store.faulted() {
                assert!(!failed, "a run fails without a fault");
                break;
            }
            for kept in 0..=store.unsynced() {
                let crashed = store.crashed(kept);
                // The process's owner cache follows one history: the crash
                // that loses every write since the sync, here; a new process
                // for the others.
                let cache = match kept {
                    0 => cache.clone(),
                    _ => cached.then(fresh_namespace),
                };
                let case = format!("{:?}: sync #{nth}, {kept} kept, cached={cached}", flow.0);
                let done = attempt(flow, &crashed.scope(&cache))?;
                assert_ended_as(&crashed.memory, &done, &expected, &case);
                cases += 1;
            }
        }
    }
    assert!(cases > 20, "{cases} cases");
    Ok(())
}

/// A crash at a sync, keeping none or all of its writes, then another during
/// the attempt that recovers from it: a third attempt still ends as without a
/// crash.
#[test]
fn a_crash_while_recovering_from_one_changes_nothing() -> Result<()> {
    for flow in FLOWS {
        crash_while_recovering(flow)?;
    }
    Ok(())
}

fn crash_while_recovering(flow: (Language, &str)) -> Result<()> {
    let expected = reference(flow)?;
    let mut attempts = 0;
    for first in 1.. {
        let store = Faulty::new();
        store.arm(first);
        let _ = attempt(flow, &store.scope(&None));
        if !store.faulted() {
            break;
        }
        for crashed in [store.crashed(0), store.crashed(store.unsynced())] {
            for second in 1.. {
                let store = Faulty::over(crashed.faults.inner.fork());
                store.arm(second);
                let recovered = attempt(flow, &store.scope(&None));
                if !store.faulted() {
                    assert_ended_as(&store.memory, &recovered?, &expected, "");
                    break;
                }
                let store = store.crashed(store.unsynced());
                let done = attempt(flow, &store.scope(&None))?;
                assert_ended_as(
                    &store.memory,
                    &done,
                    &expected,
                    &format!("{:?}: #{first}, then #{second}", flow.0),
                );
                attempts += 1;
            }
        }
    }
    assert!(attempts > 10, "{attempts} attempts");
    Ok(())
}

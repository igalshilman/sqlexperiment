//! Python programs: CPython in Wasm, saved and restored like JavaScript.
//! Needs the engine built by `make python-engine`.
use anyhow::Result;
use common::*;
use quickjs_durable::{Outcome, Status};
use serde_json::{Value, json};

mod common;

const COUNTER: &str = r#"
async def main(input, ctx):
    state = {"n": 40}
    state["self"] = state
    state["n"] += await ctx.call("increment", {"by": 2})
    await ctx.call("observe")
    return {"n": state["n"], "identity": state["self"] is state}
"#;

#[test]
fn a_new_attempt_restores_python_objects() -> Result<()> {
    let store = Store::default();
    let mut run = start_python(&store, COUNTER, Value::Null, 1000)?;
    assert_eq!(run.view().state.status, Status::Waiting);
    let first = run.view().state.pending[0].id().to_owned();
    block(run.deliver(&first, Outcome::success(2), 1001))?;
    // Another attempt: the VM is restored from the commit.
    let mut run = start_python(&store, COUNTER, Value::Null, 1002)?;
    let second = run.view().state.pending[0].id().to_owned();
    let done = block(run.deliver(&second, Outcome::success(Value::Null), 1002))?;
    assert_eq!(value(&done), json!({"n": 42, "identity": true}));
    Ok(())
}

/// A live VM, kept from turn to turn, and one restored at every turn end
/// alike, down to the bytes they store.
#[test]
fn a_live_python_run_and_a_restored_one_end_alike() -> Result<()> {
    let source = r#"
import asyncio

async def main(input, ctx):
    log = []
    for i in range(5):
        log.append(await ctx.rpc.call("S", "h", i))
        await ctx.kv.set("i", i)
    await ctx.sleep(0.01)
    both = await asyncio.gather(ctx.kv.get("i"), ctx.rpc.call("S", "h", "x"))
    return [x["input"] for x in log] + [both[1]["input"]]
"#;
    let live = Store::default();
    let mut run = start_python(&live, source, Value::Null, 1)?;
    let expected = finish(&mut run)?;
    let restored = Store::default();
    start_python(&restored, source, Value::Null, 1)?;
    loop {
        let mut run = start_python(&restored, source, Value::Null, 1)?;
        if run.view().state.outcome.is_some() {
            assert_eq!(run.view().state, expected.state);
            break;
        }
        step(&mut run)?;
    }
    assert_eq!(value(&expected), json!([0, 1, 2, 3, 4, "x"]));
    assert_eq!(restored.values(), live.values());
    Ok(())
}

#[test]
fn python_sql_commits_with_the_turn() -> Result<()> {
    let source = r#"
async def main(input, ctx):
    ctx.db.sql("CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT)")
    text = input["add"]
    ctx.db.sql(t"INSERT INTO notes (text) VALUES ({text})")
    await ctx.sleep(1)
    with ctx.db.transaction():
        ctx.db.sql("INSERT INTO notes (text) VALUES (?)", "second")
    return ctx.db.sql("SELECT id, text FROM notes ORDER BY id")
"#;
    let store = Store::default();
    let mut run = start_python(&store, source, json!({"add": "milk"}), 1)?;
    assert_eq!(
        value(&finish(&mut run)?),
        json!([{"id": 1, "text": "milk"}, {"id": 2, "text": "second"}])
    );
    Ok(())
}

#[test]
fn a_python_exception_fails_the_run() -> Result<()> {
    let source = r#"
async def main(input, ctx):
    await ctx.sleep(1)
    raise ValueError("no such thing")
"#;
    let store = Store::default();
    let mut run = start_python(&store, source, Value::Null, 1)?;
    let view = finish(&mut run)?;
    assert_eq!(view.state.status, Status::Failed);
    let error = format!("{:?}", view.state.outcome);
    assert!(
        error.contains("ValueError") && error.contains("no such thing"),
        "{error}"
    );
    Ok(())
}

/// A Python program evaluates to the same golden image on every runtime: a
/// run started in one process carries on in another.
#[test]
fn every_runtime_evaluates_a_python_program_alike() -> Result<()> {
    use quickjs_durable::{Program, Programs, Run, Runtime};
    use std::sync::Arc;
    let compiled = Program::compile_python(&runtime(), "v1", COUNTER)?;
    let again = Program::compile_python(&Arc::new(Runtime::new()?), "v1", COUNTER)?;
    let again = Arc::new(Programs::new(runtime()).with(again)?);
    let compiled = Arc::new(Programs::new(runtime()).with(compiled)?);
    let run = |catalogs: [&Arc<Programs>; 3]| -> Result<Store> {
        let store = Store::default();
        for catalog in catalogs {
            let scope = store.scope();
            let mut run = block(Run::start(
                catalog,
                &scope,
                "s",
                "r",
                "v1",
                "main",
                Value::Null,
                1,
            ))?;
            if run.view().state.outcome.is_none() {
                let operation = run.view().state.pending[0].id().to_owned();
                block(run.deliver(&operation, Outcome::success(2), 2))?;
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

/// asyncio's timers are durable sleeps: a deadline with `asyncio.timeout`
/// races a call, and the timer that loses is dropped.
#[test]
fn asyncio_timeouts_are_durable() -> Result<()> {
    let source = r#"
import asyncio

async def main(input, ctx):
    try:
        async with asyncio.timeout(5):
            await ctx.rpc.call("Slow", "never")
    except TimeoutError:
        late = True
    fast = await asyncio.wait_for(ctx.rpc.call("Fast", "now", 1), timeout=60)
    return {"late": late, "fast": fast["input"]}
"#;
    let store = Store::default();
    let run = start_python(&store, source, Value::Null, 1000)?;
    let pending = run.view().state.pending;
    assert_eq!(pending.len(), 2, "{pending:?}");
    // The timer fires first; the slow call is never answered.
    let timer = pending
        .iter()
        .find(|operation| matches!(operation, quickjs_durable::Operation::Sleep { .. }))
        .expect("a timer")
        .clone();
    let quickjs_durable::Operation::Sleep { deadline, .. } = &timer else {
        unreachable!()
    };
    assert_eq!(*deadline, 6000);
    // Restored in another attempt, the deadline still holds.
    let mut run = start_python(&store, source, Value::Null, 6000)?;
    let view = block(run.deliver(timer.id(), Outcome::success(Value::Null), 6000))?;
    let call = view
        .state
        .pending
        .iter()
        .find(|operation| matches!(operation, quickjs_durable::Operation::Rpc { target, .. } if target.service == "Fast"))
        .expect("the fast call")
        .clone();
    let done = block(run.deliver(call.id(), answer(&call), 6001))?;
    assert_eq!(value(&done), json!({"late": true, "fast": 1}));
    Ok(())
}

/// What Python cannot send as JSON fails the call that sends it, in the
/// program, where it can be caught.
#[test]
fn python_values_cross_as_json() -> Result<()> {
    let source = r#"
async def main(input, ctx):
    try:
        await ctx.kv.set("bad", {1, 2})
    except TypeError as error:
        refused = str(error)
    await ctx.kv.set("tuple", (1, "two"))
    return {"refused": refused.startswith("Expected JSON data"), "tuple": await ctx.kv.get("tuple")}
"#;
    let store = Store::default();
    let mut run = start_python(&store, source, Value::Null, 1)?;
    let done = finish(&mut run)?;
    assert_eq!(value(&done), json!({"refused": true, "tuple": [1, "two"]}));
    assert_eq!(kv(&store, "tuple"), Some(json!([1, "two"])));
    Ok(())
}

/// The outcome of Python `source`, run to its end.
fn outcome(source: &str) -> Result<quickjs_durable::View> {
    let store = Store::default();
    let mut run = start_python(&store, source, Value::Null, 1000)?;
    finish(&mut run)
}

fn failure(view: &quickjs_durable::View) -> (String, String) {
    match &view.state.outcome {
        Some(Outcome::Failure { error, .. }) => (error.name.clone(), error.message.clone()),
        other => panic!("Expected a failure, not {other:?}"),
    }
}

/// Deep recursion in C raises RecursionError, which the program can catch,
/// instead of exhausting Wasmtime's stack; so does running out of memory.
#[test]
fn python_runs_out_of_stack_and_memory_catchably() -> Result<()> {
    let source = r#"
import json

async def main(input, ctx):
    nested = []
    for _ in range(100_000):
        nested = [nested]
    caught = []
    for dump in (json.dumps, repr):
        try:
            dump(nested)
        except RecursionError:
            caught.append(dump.__name__)
    try:
        bytearray(200 * 1024 * 1024)
    except MemoryError:
        caught.append("memory")
    await ctx.sleep(0.001)
    return caught
"#;
    assert_eq!(value(&outcome(source)?), json!(["dumps", "repr", "memory"]));
    Ok(())
}

/// What a program cannot do fails as it would in Python, where it can be
/// caught: no threads, no blocking sleeps, no writes.
#[test]
fn python_has_no_threads_sleeps_or_writes() -> Result<()> {
    let source = r#"
import threading, time

async def main(input, ctx):
    errors = []
    for attempt in (
        lambda: threading.Thread(target=print).start(),
        lambda: time.sleep(0.1),
        lambda: open("/tmp/x", "w"),
    ):
        try:
            attempt()
        except (RuntimeError, OSError) as error:
            errors.append(type(error).__name__)
    return errors
"#;
    assert_eq!(
        value(&outcome(source)?),
        json!(["RuntimeError", "OSError", "OSError"])
    );
    Ok(())
}

/// The bridge loads asyncio's core only; the rest of asyncio, and the modules
/// the core defers (sockets, subprocesses, logging, thread pools), load when
/// a program uses them, as the real modules.
#[test]
fn python_loads_the_rest_of_asyncio_on_use() -> Result<()> {
    let source = r#"
import asyncio, sys

DEFERRED = ("asyncio.streams", "socket", "subprocess", "logging", "concurrent.futures", "inspect", "traceback")

async def main(input, ctx):
    # What programs use most loads none of them.
    done, _ = await asyncio.wait(
        [asyncio.ensure_future(ctx.sleep(0.001))], return_when=asyncio.FIRST_COMPLETED
    )
    await asyncio.gather(ctx.sleep(0.001), asyncio.sleep(0.001))
    async with asyncio.TaskGroup() as group:
        group.create_task(ctx.sleep(0.001))
    try:
        async with asyncio.timeout(0.001):
            await asyncio.Event().wait()
    except TimeoutError:
        pass
    task = asyncio.ensure_future(asyncio.sleep(1))
    await asyncio.sleep(0)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass
    deferred = [name in sys.modules for name in DEFERRED]
    import concurrent.futures, logging, socket, subprocess
    return {
        "deferred": deferred,
        "waited": len(done),
        "streams": asyncio.StreamReader.__module__,
        "to_thread": callable(asyncio.to_thread),
        "socket": int(socket.AF_INET6) > 0 and asyncio.events.socket.AF_INET6 == socket.AF_INET6,
        "pipe": subprocess.PIPE,
        "logger": logging.getLogger("asyncio").name,
        "pool": concurrent.futures.ThreadPoolExecutor.__name__,
    }
"#;
    assert_eq!(
        value(&outcome(source)?),
        json!({
            "deferred": [false, false, false, false, false, false, false],
            "waited": 1,
            "streams": "asyncio.streams",
            "to_thread": true,
            "socket": true,
            "pipe": -1,
            "logger": "asyncio",
            "pool": "ThreadPoolExecutor",
        })
    );
    Ok(())
}

/// `sys.exit` fails the run like any exception, and so do a value JSON
/// cannot carry exactly and a file left open across a wait.
#[test]
fn python_exits_and_bad_values_fail_the_run() -> Result<()> {
    let exit =
        "import sys\nasync def main(input, ctx):\n    await ctx.sleep(0.001)\n    sys.exit(3)\n";
    assert_eq!(failure(&outcome(exit)?), ("SystemExit".into(), "3".into()));
    let surrogate = "async def main(input, ctx):\n    return '\\ud800'\n";
    assert_eq!(failure(&outcome(surrogate)?).0, "TypeError");
    let big = "async def main(input, ctx):\n    await ctx.kv.set('n', 2**70)\n";
    let (name, message) = failure(&outcome(big)?);
    assert!(
        name == "TypeError" && message.contains("beyond 2**53"),
        "{message}"
    );
    let huge = "async def main(input, ctx):\n    return 'x' * (2 * 1024 * 1024)\n";
    assert_eq!(failure(&outcome(huge)?).0, "TypeError");
    let open = "async def main(input, ctx):\n    f = open('/lib/python3.15/os.py')\n    await ctx.sleep(0.001)\n    return f.readline()\n";
    let (_, message) = failure(&outcome(open)?);
    assert!(
        message.contains("A file is open across a wait"),
        "{message}"
    );
    Ok(())
}

/// Async generators, context variables and task groups live across waits.
#[test]
fn python_async_generators_and_task_groups_survive_waits() -> Result<()> {
    let source = r#"
import asyncio, contextvars

factor = contextvars.ContextVar("factor")

async def main(input, ctx):
    async def counted():
        for k in range(3):
            await ctx.sleep(0.001)
            yield k * factor.get()

    factor.set(10)
    seen = [x async for x in counted()]

    async def boom():
        await ctx.sleep(0.001)
        raise ValueError("boom")

    try:
        async with asyncio.TaskGroup() as group:
            group.create_task(boom())
            group.create_task(ctx.rpc.call("S", "h", 1))
    except* ValueError as errors:
        caught = [str(error) for error in errors.exceptions]
    return {"seen": seen, "caught": caught}
"#;
    let store = Store::default();
    start_python(&store, source, Value::Null, 1)?;
    // Every turn on a restored VM.
    loop {
        let mut run = start_python(&store, source, Value::Null, 1)?;
        if run.view().state.outcome.is_some() {
            assert_eq!(
                value(&run.view()),
                json!({"seen": [0, 10, 20], "caught": ["boom"]})
            );
            return Ok(());
        }
        step(&mut run)?;
    }
}

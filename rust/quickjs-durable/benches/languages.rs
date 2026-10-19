//! The same workloads in JavaScript and in Python: `cargo bench --bench
//! languages` (Python needs `make python-engine`). Each run goes once with its VM live
//! from turn to turn, and once restoring it at every turn, as a new attempt
//! would. Prints a table: the cost of a language is its golden image, its
//! VM starts and the pages its turns change.
use anyhow::{Result, ensure};
use quickjs_durable::{Language, Outcome, Program, Programs, Run, Runtime, Scope, Status};
use serde_json::{Value, json};
use sqlite_kv::simulation::MemoryKv;
use std::{sync::Arc, time::Instant};

const CASES: &[(&str, &str, &str, i64)] = &[
    (
        "scalar",
        "export async function main(_, ctx) { let n = 0; for (let i = 0; i < 3; i++) { n += i; await ctx.call('observe'); } return n; }",
        "async def main(input, ctx):\n    n = 0\n    for i in range(3):\n        n += i\n        await ctx.call('observe')\n    return n\n",
        3,
    ),
    (
        "heap-churn",
        "export async function main(_, ctx) { let a; for (let i = 0; i < 3; i++) { a = Array.from({length: 2000}, (_, j) => ({id: j, value: 'item-' + i + '-' + j})); await ctx.call('observe'); } return a.length; }",
        "async def main(input, ctx):\n    for i in range(3):\n        a = [{'id': j, 'value': f'item-{i}-{j}'} for j in range(2000)]\n        await ctx.call('observe')\n    return len(a)\n",
        2000,
    ),
];

fn main() -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "Run with cargo bench --bench languages"
    );
    let runtime = Arc::new(Runtime::new()?);
    println!(
        "language   workload    golden_ms  image_kb  restored  turn  turn_ms  changed_pages  batch_kb"
    );
    for (name, javascript, python, expected) in CASES {
        for (language, source) in [
            (Language::JavaScript, javascript),
            (Language::Python, python),
        ] {
            let before = Instant::now();
            let program = Program::compile_in(&runtime, language, name, source)?;
            let golden_ms = before.elapsed().as_secs_f64() * 1000.0;
            let programs = Arc::new(Programs::new(runtime.clone()).with(program)?);
            for restored in [false, true] {
                let store = MemoryKv::default();
                let scope = Scope::new(Arc::new(store.clone()));
                let start = || {
                    Run::start(
                        &programs,
                        &scope,
                        "scope",
                        "bench",
                        name,
                        "main",
                        Value::Null,
                        1000,
                    )
                };
                let started = Instant::now();
                let mut run = futures_executor::block_on(start())?;
                let mut turn_ms = started.elapsed().as_secs_f64() * 1000.0;
                loop {
                    let view = run.view();
                    println!(
                        "{:<10} {:<11} {:>9.1} {:>9} {:>9} {:>5} {:>8.2} {:>14} {:>9.1}",
                        format!("{language:?}"),
                        name,
                        golden_ms,
                        view.memory_bytes / 1024,
                        restored,
                        view.turns,
                        turn_ms,
                        view.changed_pages,
                        view.batch_bytes as f64 / 1024.0,
                    );
                    if view.state.status != Status::Waiting {
                        ensure!(
                            view.state.outcome == Some(Outcome::success(json!(expected))),
                            "Wrong result: {:?}",
                            view.state
                        );
                        break;
                    }
                    let operation = view.state.pending[0].id().to_owned();
                    let started = Instant::now();
                    if restored {
                        run = futures_executor::block_on(start())?;
                    }
                    futures_executor::block_on(run.deliver(
                        &operation,
                        Outcome::success(Value::Null),
                        1001 + view.turns,
                    ))?;
                    turn_ms = started.elapsed().as_secs_f64() * 1000.0;
                }
            }
        }
    }
    Ok(())
}

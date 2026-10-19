//! A program's exported functions, each a handler of its object.
use anyhow::Result;
use common::*;
use quickjs_durable::{Outcome, Programs};
use serde_json::{Value, json};

mod common;

/// What a call ended with: its value, or its error's name.
fn ended(run: &mut quickjs_durable::Run) -> Result<Value> {
    Ok(match finish(run)?.state.outcome {
        Some(Outcome::Success { value, .. }) => value,
        Some(Outcome::Failure { error, .. }) => json!({"error": error.name}),
        None => unreachable!(),
    })
}

const COUNTER: &str = r#"
  let calls = 0;
  export async function add(n, ctx) {
    calls++;
    const count = ((await ctx.kv.get('count')) ?? 0) + n;
    await ctx.kv.set('count', count);
    await ctx.sleep(1);
    return { count, calls };
  }
  export async function get(_, ctx) {
    return await ctx.kv.get('count');
  }
"#;

/// The module's variables live on from call to call, beside the scope's KV.
#[test]
fn calls_share_the_scope_and_its_memory() -> Result<()> {
    let store = Store::default();
    let first = ended(&mut call(&store, COUNTER, "a", "add", json!(2))?)?;
    assert_eq!(first, json!({"count": 2, "calls": 1}));
    let second = ended(&mut call(&store, COUNTER, "b", "add", json!(3))?)?;
    assert_eq!(second, json!({"count": 5, "calls": 2}));
    assert_eq!(
        ended(&mut call(&store, COUNTER, "c", "get", Value::Null)?)?,
        json!(5)
    );
    Ok(())
}

#[test]
fn only_exported_functions_are_handlers() -> Result<()> {
    let source = "function hidden() { return 1 } export const value = 2; export function shown() { return hidden() }";
    assert_eq!(programs(source).get("v1")?.handlers(), ["shown"]);
    let store = Store::default();
    let mut hidden = call(&store, source, "a", "hidden", Value::Null)?;
    assert_eq!(ended(&mut hidden)?, json!({"error": "TypeError"}));
    let mut shown = call(&store, source, "b", "shown", Value::Null)?;
    assert_eq!(ended(&mut shown)?, json!(1));
    Ok(())
}

/// Another attempt at a call is of the same handler.
#[test]
fn a_run_is_a_call_of_one_handler() -> Result<()> {
    let store = Store::default();
    call(&store, COUNTER, "a", "add", json!(1))?;
    let error = call(&store, COUNTER, "a", "get", Value::Null)
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("is a call of v1.add"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn a_program_must_compile_and_export_a_function() {
    let compile = |source: &str| {
        Programs::new(runtime())
            .compile("bad", source)
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default()
    };
    assert!(compile("export const x = 1;").contains("exports at least one function"));
    assert!(compile("export function (").contains("does not compile"));
    assert!(compile("throw new Error('at load')").contains("at load"));
    assert!(compile("export function $bad() {}").contains("not a handler name"));
}

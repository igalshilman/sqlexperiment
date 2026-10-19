//! Objects: a program's module per key, whose memory lives on from call to
//! call. Each exported function is a handler.
use anyhow::Result;
use common::*;
use quickjs_durable::{Outcome, Programs, Status};
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

const TALLY: &str = r#"
  class Tally {
    #counts = new Map();
    add(word) { this.#counts.set(word, (this.#counts.get(word) ?? 0) + 1); }
    get top() { return [...this.#counts].sort((a, b) => b[1] - a[1])[0]; }
  }
  const tally = new Tally();
  let calls = 0;

  export async function add(words, ctx) {
    calls++;
    for (const word of words) {
      tally.add(word);
      await ctx.sleep(1);
    }
    return calls;
  }
  export function top() { return tally.top; }
  export async function spin() { calls = 1000; while (true) {} }
"#;

#[test]
fn memory_lives_on_from_call_to_call() -> Result<()> {
    let store = Store::default();
    assert_eq!(
        ended(&mut call(&store, TALLY, "a", "add", json!(["x", "y"]))?)?,
        json!(1)
    );
    assert_eq!(
        ended(&mut call(&store, TALLY, "b", "add", json!(["y"]))?)?,
        json!(2)
    );
    assert_eq!(
        ended(&mut call(&store, TALLY, "c", "top", Value::Null)?)?,
        json!(["y", 2])
    );
    Ok(())
}

/// A trap undoes its turn: the next call finds the memory as last committed.
#[test]
fn a_trap_leaves_the_memory_as_it_was() -> Result<()> {
    let store = Store::default();
    ended(&mut call(&store, TALLY, "a", "add", json!(["x"]))?)?;
    let mut spun = call(&store, TALLY, "b", "spin", Value::Null)?;
    assert_eq!(ended(&mut spun)?, json!({"error": "Trap"}));
    assert_eq!(
        ended(&mut call(&store, TALLY, "c", "add", json!([]))?)?,
        json!(2)
    );
    Ok(())
}

/// A call that never finished (cancelled, say) does not hold the object: the
/// next call runs on the memory it committed, and its requests are dropped.
#[test]
fn a_call_left_waiting_does_not_block_the_next() -> Result<()> {
    let store = Store::default();
    let waiting = call(&store, TALLY, "a", "add", json!(["x", "y"]))?;
    assert_eq!(waiting.view().state.status, Status::Waiting);
    assert_eq!(
        ended(&mut call(&store, TALLY, "b", "top", Value::Null)?)?,
        json!(["x", 1])
    );
    let mut next = call(&store, TALLY, "c", "add", json!([]))?;
    assert_eq!(ended(&mut next)?, json!(2));
    Ok(())
}

#[test]
fn only_exported_functions_are_handlers() -> Result<()> {
    let source = "function hidden() { return 1 } export const value = 2; export function shown() { return hidden() }";
    let programs = programs(source);
    assert_eq!(programs.get("v1")?.handlers(), ["shown"]);
    let store = Store::default();
    let mut hidden = call(&store, source, "a", "hidden", Value::Null)?;
    assert_eq!(ended(&mut hidden)?, json!({"error": "TypeError"}));
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
}

//! A long coding session of the shipped `coder` program, over a store in
//! memory: `cargo bench --bench agent`. A scripted model reads files, runs
//! commands and edits, step after step, with outputs the size of real ones,
//! until the conversation is long. Every 25 steps the run is restored, as a
//! new attempt after a crash would be. Results go to bench-agent.json.
//!
//! Per step it reports the request the run sends the model (the whole
//! conversation, which the host reads from the run's heap: neither the
//! commit nor Restate's journal keeps it), the time of the step's two turns
//! (the model's answer in, the tool's result in), and what the step stored.
use anyhow::{Result, bail, ensure};
use functions::{Operation, Outcome, Programs, Run, Runtime, Scope, Status};
use futures_executor::block_on as block;
use serde_json::{Value, json};
use sqlite_kv::simulation::MemoryKv;
use std::{sync::Arc, time::Instant};

const STEPS: usize = 300;
const RESTORE_EVERY: usize = 25;

fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        println!("Use cargo bench --bench agent for optimized measurements");
        return Ok(());
    }
    let source = include_str!("../../../examples-functions/coder.js");
    let runtime = Arc::new(Runtime::new()?);
    let programs = Arc::new(Programs::new(runtime).compile("coder", source)?);
    let store = MemoryKv::default();
    let scope = Scope::new(Arc::new(store.clone()));
    let start = |now| {
        let task = json!({"task": "Refactor the parser"});
        block(Run::start(
            &programs, &scope, "bench", "run", "coder", "work", task, now,
        ))
    };
    let mut run = start(1000)?;
    let mut session = Session::default();
    let mut stored = store.stats().bytes_written as usize;
    let mut rows = Vec::new();
    let mut ended = None;
    println!("step  request KiB  model ms  tool ms  stored B/step  of it, commit B  stored MiB");
    'session: for step in 1..=STEPS {
        // The model's answer, then the tool's result.
        let mut turns = [0.0; 2];
        let mut request = 0;
        for turn in &mut turns {
            let view = run.view();
            if view.state.status != Status::Waiting {
                let outcome = view.state.outcome;
                println!("The run ended at step {step}: {outcome:?}");
                ended = Some(json!({"step": step, "outcome": outcome}));
                break 'session;
            }
            let operation = view.state.pending[0].clone();
            let answer = match &operation {
                Operation::Call { id, name, .. } if name == "claude.messages" => {
                    let input = block(run.input(id))?;
                    request =
                        serde_json::to_vec(&input.map_err(|e| anyhow::anyhow!(e.message))?)?.len();
                    session.model(step)
                }
                Operation::Rpc { target, .. } => session.tool(&target.handler),
                _ => bail!("Step {step}: unexpected request {operation:?}"),
            };
            let id = operation.id();
            let started = Instant::now();
            block(run.deliver(id, Outcome::success(answer), view.state.now + 1))?;
            *turn = started.elapsed().as_secs_f64() * 1000.0;
        }
        let written = store.stats().bytes_written as usize - stored;
        stored = store.stats().bytes_written as usize;
        let mut restore_ms = None;
        if step.is_multiple_of(RESTORE_EVERY) {
            let started = Instant::now();
            run = start(run.view().state.now)?;
            restore_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
        }
        let view = run.view();
        rows.push(
            json!({"step": step, "request_bytes": request, "model_turn_ms": turns[0],
            "tool_turn_ms": turns[1], "restore_ms": restore_ms, "stored_bytes": written,
            "memory_bytes": view.memory_bytes,
            "live_pages": view.live_pages, "batch_bytes": view.batch_bytes,
            "commit_bytes": view.commit_bytes}),
        );
        if step.is_multiple_of(10) || step == 1 {
            println!(
                "{step:>4}  {:>11.0}  {:>8.2}  {:>7.2}  {:>13}  {:>15}  {:>10.1}",
                request as f64 / 1024.0,
                turns[0],
                turns[1],
                written,
                view.commit_bytes,
                stored as f64 / (1 << 20) as f64,
            );
        }
    }
    if ended.is_none() {
        let view = run.view();
        let done =
            json!({"content": [{"type": "text", "text": "Done."}], "stop_reason": "end_turn"});
        let id = view.state.pending[0].id();
        let view = block(run.deliver(id, Outcome::success(done), view.state.now + 1))?;
        ensure!(
            view.state.status == Status::Completed,
            "The session did not complete: {:?}",
            view.state.outcome
        );
    }
    let output = json!({"steps": STEPS, "ended": ended, "notes": [
        "request_bytes: the step's request to the model, the whole conversation so far, read from the heap; nothing stores it but the heap.",
        "stored_bytes: what the step's two turns stored (batches, commits); Scope, no network.",
        "restore_ms: a new attempt after the step, reading the commit and its batches into a new VM."],
        "rows": rows});
    std::fs::write("bench-agent.json", serde_json::to_string_pretty(&output)?)?;
    Ok(())
}

/// The scripted session: per step, a few hundred bytes of the model's text and
/// one tool, in turn reading a file (~6 KiB), running a command (~1.5 KiB of
/// output), editing a file and writing one (~3 KiB). Its text varies, as code
/// does, so that neither compression nor caches flatter it.
#[derive(Default)]
struct Session {
    seed: u64,
}

impl Session {
    fn model(&mut self, step: usize) -> Value {
        let input = match step % 4 {
            0 => json!({"path": format!("src/module_{step}.rs")}),
            1 => json!({"command": "cargo test 2>&1 | tail -40"}),
            2 => json!({"path": "src/lib.rs", "old_text": self.code(4), "new_text": self.code(5)}),
            _ => json!({"path": format!("src/new_{step}.rs"), "content": self.code(60)}),
        };
        let name = ["read_file", "bash", "edit_file", "write_file"][step % 4];
        json!({
            "content": [
                {"type": "text", "text": self.prose(400)},
                {"type": "tool_use", "id": format!("toolu_{step}"), "name": name, "input": input},
            ],
            "stop_reason": "tool_use",
        })
    }

    fn tool(&mut self, handler: &str) -> Value {
        match handler {
            "read" => json!(self.code(120)),
            "bash" => json!({"exit": 0, "output": self.prose(1500)}),
            "edit" => json!("Edited src/lib.rs"),
            _ => json!("Wrote 3000 bytes"),
        }
    }

    fn next(&mut self) -> u64 {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.seed >> 33
    }

    /// `lines` lines of code-like text, ~50 bytes each.
    fn code(&mut self, lines: usize) -> String {
        (0..lines)
            .map(|_| {
                let (f, a, b) = (self.next() % 10_000, self.next() % 997, self.next());
                format!(
                    "    let v{f} = x.wrapping_mul({a}) ^ 0x{:x};\n",
                    b & 0xffff_ffff
                )
            })
            .collect()
    }

    /// About `bytes` bytes of words.
    fn prose(&mut self, bytes: usize) -> String {
        const WORDS: [&str; 16] = [
            "the", "test", "parser", "fails", "because", "token", "span", "is", "off", "by", "one",
            "when", "input", "ends", "with", "newline",
        ];
        let mut text = String::new();
        while text.len() < bytes {
            text.push_str(WORDS[(self.next() % 16) as usize]);
            text.push(if self.next().is_multiple_of(12) {
                '\n'
            } else {
                ' '
            });
        }
        text
    }
}

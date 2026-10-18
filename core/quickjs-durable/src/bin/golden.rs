//! Compute programs' golden dumps: each `<name>.js` given is evaluated once,
//! and its memory image written beside it as `<name>.golden`. A dump is valid
//! for this build of the runtime and that source only; compute it again after
//! changing either.
//!
//! ```sh
//! cargo run --release --bin golden -- programs/*.js
//! ```
use anyhow::{Context, Result, ensure};
use quickjs_durable::{Program, Runtime};
use std::path::Path;

fn main() -> Result<()> {
    let sources: Vec<String> = std::env::args().skip(1).collect();
    ensure!(!sources.is_empty(), "Usage: golden <program.js>...");
    let runtime = Runtime::new()?;
    for source in sources {
        let path = Path::new(&source);
        ensure!(
            path.extension().is_some_and(|e| e == "js"),
            "{source}: a program is a .js file"
        );
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("Invalid program file name")?;
        let text = std::fs::read_to_string(path).with_context(|| format!("Reading {source}"))?;
        let program = Program::compile(&runtime, name, &text)
            .with_context(|| format!("Compiling {source}"))?;
        let dump = program.dump()?;
        let target = path.with_extension("golden");
        std::fs::write(&target, &dump).with_context(|| format!("Writing {}", target.display()))?;
        println!("{} ({} KiB)", target.display(), dump.len() / 1024);
    }
    Ok(())
}

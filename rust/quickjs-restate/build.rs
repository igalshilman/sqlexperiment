//! Embed the programs: each `<name>.js` in `QJS_PROGRAMS` (by default
//! `programs/`), with its golden dump `<name>.golden` beside it, which
//! quickjs-durable's `golden` binary computes (`make goldens`).
use std::{fmt::Write, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=QJS_PROGRAMS");
    let dir = std::env::var_os("QJS_PROGRAMS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("programs"));
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "js"))
        .collect();
    sources.sort();
    let mut table = String::from("pub const PROGRAMS: &[(&str, &str, &[u8])] = &[\n");
    for source in sources {
        let source = source.canonicalize().unwrap();
        let golden = source.with_extension("golden");
        assert!(
            golden.exists(),
            "{}: no golden dump; compute it with `make goldens`",
            golden.display()
        );
        println!("cargo:rerun-if-changed={}", source.display());
        println!("cargo:rerun-if-changed={}", golden.display());
        let name = source.file_stem().unwrap().to_str().unwrap();
        writeln!(
            table,
            "    ({name:?}, include_str!({:?}), include_bytes!({:?})),",
            source.display().to_string(),
            golden.display().to_string()
        )
        .unwrap();
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("programs.rs");
    std::fs::write(out, table).unwrap();
}

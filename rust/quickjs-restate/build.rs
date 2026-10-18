//! Ship the programs: each `<name>.js` in `QJS_PROGRAMS` (by default
//! `programs/`) is evaluated here, once per build, into its golden memory
//! image; the binary embeds its source and that image's dump. Edit a program
//! and `cargo build` again.
use quickjs_durable::{Program, Runtime};
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
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let runtime = Runtime::new().expect("QuickJS runtime");
    let mut table = String::from("pub const PROGRAMS: &[(&str, &str, &[u8])] = &[\n");
    for source in sources {
        println!("cargo:rerun-if-changed={}", source.display());
        let source = source.canonicalize().unwrap();
        let name = source.file_stem().unwrap().to_str().unwrap();
        let text = std::fs::read_to_string(&source).unwrap();
        let program = Program::compile(&runtime, name, &text)
            .unwrap_or_else(|e| panic!("{}: {e:#}", source.display()));
        let dump = out.join(format!("{name}.golden"));
        std::fs::write(&dump, program.dump().unwrap()).unwrap();
        writeln!(
            table,
            "    ({name:?}, include_str!({:?}), include_bytes!({:?})),",
            source.display().to_string(),
            dump.display().to_string()
        )
        .unwrap();
    }
    table.push_str("];\n");
    std::fs::write(out.join("programs.rs"), table).unwrap();
}

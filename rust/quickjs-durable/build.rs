//! Ship the durable functions: the binary embeds each `<name>.js` and
//! `<name>.py` in `QJS_PROGRAMS` (by default the repository's
//! `examples-functions/`), and evaluates it when it starts.
use std::{fmt::Write, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=QJS_PROGRAMS");
    let dir = std::env::var_os("QJS_PROGRAMS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples-functions")
        });
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "js" || e == "py"))
        .collect();
    sources.sort();
    let mut table = String::from("pub const PROGRAMS: &[(&str, crate::Language, &str)] = &[\n");
    for source in sources {
        println!("cargo:rerun-if-changed={}", source.display());
        let source = source.canonicalize().unwrap();
        let name = source.file_stem().unwrap().to_str().unwrap();
        let language = match source.extension().unwrap().to_str().unwrap() {
            "js" => "JavaScript",
            _ => "Python",
        };
        writeln!(
            table,
            "    ({name:?}, crate::Language::{language}, include_str!({:?})),",
            source.display().to_string()
        )
        .unwrap();
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("programs.rs"), table).unwrap();
}

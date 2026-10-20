//! Programs: static JavaScript or Python, each evaluated once into a golden
//! memory image that every run of it starts from and every checkpoint is
//! relative to.
//!
//! Evaluation is deterministic (a fixed identity, clock and seed): the same
//! source on the same runtime gives the same image, byte for byte, on every
//! start of every process, so checkpoints written by one restore in another.
use crate::{
    model::{Image, MAX_INPUT, fingerprint},
    vm::{Language, Runtime},
};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, sync::Arc};

/// A program: its name, the golden image of its evaluated module, and the
/// functions it exports, each a handler of its object.
pub struct Program {
    name: String,
    language: Language,
    version: u64,
    image: Image,
    handlers: Vec<String>,
}

impl Program {
    /// Evaluate JavaScript `source`, a module exporting functions
    /// `(input, ctx)`.
    pub fn compile(runtime: &Runtime, name: &str, source: &str) -> Result<Self> {
        Self::compile_in(runtime, Language::JavaScript, name, source)
    }

    /// Evaluate Python `source`, a module defining functions
    /// `async def handler(input, ctx)`.
    pub fn compile_python(runtime: &Runtime, name: &str, source: &str) -> Result<Self> {
        Self::compile_in(runtime, Language::Python, name, source)
    }

    pub fn compile_in(
        runtime: &Runtime,
        language: Language,
        name: &str,
        source: &str,
    ) -> Result<Self> {
        identifier(name)?;
        ensure!(source.len() <= MAX_INPUT, "Program source exceeds limit");
        let (image, handlers) = runtime
            .golden(language, name, source)
            .map_err(|error| anyhow::anyhow!("Program {name}: {error:#}"))?;
        for handler in &handlers {
            ensure!(
                handler.starts_with(|c: char| c.is_ascii_alphabetic())
                    && handler
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "Program {name}: the export {handler} is not a handler name (letters, digits and _)"
            );
            ensure!(
                !crate::restate::BUILT_IN.contains(&handler.as_str()),
                "Program {name}: {handler} is a handler every object has (see restate.rs): name the export otherwise"
            );
        }
        Ok(Self {
            name: name.into(),
            language,
            version: fingerprint(source.as_bytes()),
            image,
            handlers,
        })
    }

    /// The functions the program exports: its object's handlers.
    pub fn handlers(&self) -> &[String] {
        &self.handlers
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// The version of the program: its source's fingerprint. A run's
    /// checkpoints are relative to this version's image, and restore only with it.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The golden image: shared, not copied.
    pub(crate) fn image(&self) -> Image {
        self.image.clone()
    }
}

/// The programs a host serves, by name, on one runtime.
pub struct Programs {
    runtime: Arc<Runtime>,
    programs: HashMap<String, Program>,
}

impl Programs {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            programs: HashMap::new(),
        }
    }

    /// Add a program compiled from JavaScript `source`.
    pub fn compile(self, name: &str, source: &str) -> Result<Self> {
        let program = Program::compile(&self.runtime, name, source)?;
        self.with(program)
    }

    /// Add a program compiled from Python `source`.
    pub fn compile_python(self, name: &str, source: &str) -> Result<Self> {
        let program = Program::compile_python(&self.runtime, name, source)?;
        self.with(program)
    }

    /// Add `program`; its name must be new.
    pub fn with(mut self, program: Program) -> Result<Self> {
        ensure!(
            !self.programs.contains_key(&program.name),
            "Program {} is already defined",
            program.name
        );
        self.programs.insert(program.name.clone(), program);
        Ok(self)
    }

    pub fn get(&self, name: &str) -> Result<&Program> {
        self.programs
            .get(name)
            .with_context(|| format!("No program {name}"))
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.programs.keys().map(String::as_str)
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }
}

fn identifier(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "A program name is letters, digits, - and _"
    );
    Ok(())
}

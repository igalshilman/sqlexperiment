//! Programs: static JavaScript, each evaluated once into a golden memory image
//! that every run of it starts from and every checkpoint is relative to.
//!
//! Evaluation is deterministic (a fixed identity, clock and seed): the same
//! source on the same runtime gives the same image, byte for byte, on every
//! start of every process, so checkpoints written by one restore in another.
use crate::{
    model::{Image, MAX_INPUT, VmMeta, hash},
    vm::Runtime,
};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, sync::Arc};

/// A program: its name, and the golden image of its evaluated source.
pub struct Program {
    name: String,
    source_hash: String,
    meta: VmMeta,
    memory: Arc<Vec<u8>>,
}

impl Program {
    /// Evaluate `source`, which must define `async function main(input, ctx)`.
    pub fn compile(runtime: &Runtime, name: &str, source: &str) -> Result<Self> {
        identifier(name)?;
        ensure!(source.len() <= MAX_INPUT, "Program source exceeds limit");
        let image = runtime.golden(name, source)?;
        Ok(Self {
            name: name.into(),
            source_hash: hash(source.as_bytes()),
            meta: image.meta,
            memory: Arc::new(image.memory),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The hash of the program's source: a run's checkpoints are relative to
    /// this version's image, and restore only with it.
    pub fn source_hash(&self) -> &str {
        &self.source_hash
    }

    pub(crate) fn memory(&self) -> &Arc<Vec<u8>> {
        &self.memory
    }

    pub(crate) fn image(&self) -> Image {
        Image {
            meta: self.meta.clone(),
            memory: self.memory.to_vec(),
        }
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

    /// Add a program compiled from `source`.
    pub fn compile(self, name: &str, source: &str) -> Result<Self> {
        let program = Program::compile(&self.runtime, name, source)?;
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

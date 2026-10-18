//! Programs: static JavaScript, each evaluated once into a golden memory image
//! that every run of it starts from and every checkpoint is relative to.
//!
//! A program's dump is its golden image, computed ahead of time (such as by a
//! build script) and shipped with its source. It is valid for one build of the
//! runtime and one source: loading it with another refuses.
use crate::{
    codec,
    model::{Image, MAX_INPUT, MAX_MEMORY, VmMeta, hash},
    vm::Runtime,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

const MAGIC: &[u8; 8] = b"QJSDUMP1";

/// A program: its name, and the golden image of its evaluated source.
pub struct Program {
    name: String,
    header: Header,
    memory: Arc<Vec<u8>>,
}

/// What a dump records about its image, ahead of the compressed memory.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    source_hash: String,
    build: String,
    memory_len: usize,
    meta: VmMeta,
}

impl Program {
    /// Evaluate `source`, which must define `async function main(input, ctx)`.
    pub fn compile(runtime: &Runtime, name: &str, source: &str) -> Result<Self> {
        identifier(name)?;
        ensure!(source.len() <= MAX_INPUT, "Program source exceeds limit");
        let image = runtime.golden(name, source)?;
        Ok(Self {
            name: name.into(),
            header: Header {
                source_hash: hash(source.as_bytes()),
                build: runtime.build.clone(),
                memory_len: image.memory.len(),
                meta: image.meta,
            },
            memory: Arc::new(image.memory),
        })
    }

    /// Load a dump of `source` written by [`Program::dump`].
    pub fn load(runtime: &Runtime, name: &str, source: &str, dump: &[u8]) -> Result<Self> {
        identifier(name)?;
        let rest = dump.strip_prefix(MAGIC).context("Not a program dump")?;
        let (length, rest) = rest.split_at_checked(4).context("Truncated program dump")?;
        let length = u32::from_le_bytes(length.try_into()?) as usize;
        let (header, memory) = rest
            .split_at_checked(length)
            .context("Truncated program dump")?;
        let header: Header = serde_json::from_slice(header)?;
        ensure!(
            header.build == runtime.build,
            "The dump of {name} is for another build of the runtime: compute it again"
        );
        ensure!(
            header.source_hash == hash(source.as_bytes()),
            "The dump of {name} is of another source: compute it again"
        );
        ensure!(
            header.memory_len <= MAX_MEMORY && header.memory_len.is_multiple_of(65536),
            "Invalid golden memory size"
        );
        let memory = codec::decompress(memory, header.memory_len)?;
        ensure!(
            memory.len() == header.memory_len,
            "Golden image size mismatch"
        );
        Ok(Self {
            name: name.into(),
            header,
            memory: Arc::new(memory),
        })
    }

    /// The dump: a header, then the golden memory, compressed.
    pub fn dump(&self) -> Result<Vec<u8>> {
        let header = serde_json::to_vec(&self.header)?;
        let mut dump = MAGIC.to_vec();
        dump.extend_from_slice(&u32::try_from(header.len())?.to_le_bytes());
        dump.extend_from_slice(&header);
        dump.extend_from_slice(&codec::compress(&self.memory)?);
        Ok(dump)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The hash of the program's source: a run's checkpoints are relative to
    /// this version's image, and restore only with it.
    pub fn source_hash(&self) -> &str {
        &self.header.source_hash
    }

    pub(crate) fn memory(&self) -> &Arc<Vec<u8>> {
        &self.memory
    }

    pub(crate) fn image(&self) -> Image {
        Image {
            meta: self.header.meta.clone(),
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

    /// Add a program from its dump.
    pub fn load(self, name: &str, source: &str, dump: &[u8]) -> Result<Self> {
        let program = Program::load(&self.runtime, name, source, dump)?;
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

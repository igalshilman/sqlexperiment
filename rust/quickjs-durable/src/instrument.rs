//! Portable write barriers for the pinned wasm32 QuickJS reactor.
//!
//! The input is validated with a deliberately restricted feature set. Every
//! supported memory-writing instruction is rewritten, including bulk writes.
//! A second, fixed memory contains one byte per 4 KiB application page. Guest
//! addresses still refer to memory zero, so the tracker cannot alias the heap.
//! We mark *after* successful stores: traps discard the entire turn and bitmap.
//! This preserves out-of-bounds trapping, including address+offset overflow.
//! The generated tracking stores are never recursively instrumented.
use anyhow::{Result, ensure};
use wasm_encoder::{
    self as enc, Instruction as I,
    reencode::{self, Reencode},
};

use wasmparser as p;

pub(crate) const EXPORT: &str = "__durable_dirty_pages";
pub(crate) const TRACKER_MEMORY: u32 = 1;

type EncodeResult<T> = std::result::Result<T, reencode::Error<String>>;

/// Instrument once before compilation. This is intentionally not a generic
/// Wasm transform: shared/memory64/imported memories, SIMD, atomics and GC are
/// rejected. Extending the allowed features requires extending write coverage.
pub(crate) fn instrument(wasm: &[u8]) -> Result<Vec<u8>> {
    let features = p::WasmFeatures::WASM1
        | p::WasmFeatures::BULK_MEMORY
        | p::WasmFeatures::SIGN_EXTENSION
        | p::WasmFeatures::SATURATING_FLOAT_TO_INT
        | p::WasmFeatures::MULTI_VALUE
        | p::WasmFeatures::REFERENCE_TYPES;
    p::Validator::new_with_features(features).validate_all(wasm)?;
    let mut params = Vec::new();
    let mut functions = Vec::new();
    let mut memories = 0;
    let mut has_export_section = false;
    for payload in p::Parser::new(0).parse_all(wasm) {
        match payload? {
            p::Payload::TypeSection(types) => {
                for ty in types.into_iter_err_on_gc_types() {
                    params.push(ty?.params().len() as u32);
                }
            }
            p::Payload::FunctionSection(types) => {
                for ty in types {
                    functions.push(params[ty? as usize]);
                }
            }
            p::Payload::MemorySection(types) => {
                for ty in types {
                    let ty = ty?;
                    ensure!(
                        !ty.memory64 && !ty.shared,
                        "Tracking requires private wasm32 memory"
                    );
                    ensure!(
                        ty.initial <= (crate::model::MAX_MEMORY / 65536) as u64,
                        "Initial memory exceeds limit"
                    );
                    memories += 1;
                }
            }
            p::Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    ensure!(
                        !matches!(import?.ty, p::TypeRef::Memory(_)),
                        "Imported memory is unsupported"
                    );
                }
            }
            p::Payload::ExportSection(exports) => {
                has_export_section = true;
                for export in exports {
                    ensure!(export?.name != EXPORT, "Reserved tracking export");
                }
            }
            _ => {}
        }
    }
    ensure!(
        memories == 1 && has_export_section,
        "Expected one defined memory and an export section"
    );
    let mut encoder = Instrument {
        functions,
        next_function: 0,
    };
    let mut module = enc::Module::new();
    encoder
        .parse_core_module(&mut module, p::Parser::new(0), wasm)
        .map_err(|e| anyhow::anyhow!("Instrumenting QuickJS: {e}"))?;
    Ok(module.finish())
}

struct Instrument {
    functions: Vec<u32>,
    next_function: usize,
}
impl Reencode for Instrument {
    type Error = String;
    fn memory_type(&mut self, ty: p::MemoryType) -> EncodeResult<enc::MemoryType> {
        let mut ty = reencode::utils::memory_type(self, ty);
        let maximum = (crate::model::MAX_MEMORY / 65536) as u64;
        ty.maximum = Some(ty.maximum.unwrap_or(maximum).min(maximum));
        Ok(ty)
    }
    fn parse_memory_section(
        &mut self,
        out: &mut enc::MemorySection,
        input: p::MemorySectionReader<'_>,
    ) -> EncodeResult<()> {
        reencode::utils::parse_memory_section(self, out, input)?;
        // One 64 KiB Wasm page holds 16,384 tracking bytes for the 64 MiB limit.
        out.memory(enc::MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        Ok(())
    }
    fn parse_export_section(
        &mut self,
        out: &mut enc::ExportSection,
        input: p::ExportSectionReader<'_>,
    ) -> EncodeResult<()> {
        reencode::utils::parse_export_section(self, out, input)?;
        out.export(EXPORT, enc::ExportKind::Memory, TRACKER_MEMORY);
        Ok(())
    }
    fn parse_function_body(
        &mut self,
        code: &mut enc::CodeSection,
        body: p::FunctionBody<'_>,
    ) -> EncodeResult<()> {
        let mut locals = Vec::new();
        let mut base = self.functions[self.next_function];
        self.next_function += 1;
        for local in body.get_locals_reader()? {
            let (count, ty) = local?;
            base = base
                .checked_add(count)
                .ok_or_else(|| reencode::Error::UserError("Too many locals".into()))?;
            locals.push((count, self.val_type(ty)?));
        }
        // address, i32 value/source, bulk length, then typed store values.
        locals.extend([
            (3, enc::ValType::I32),
            (1, enc::ValType::I64),
            (1, enc::ValType::F32),
            (1, enc::ValType::F64),
        ]);
        let mut f = enc::Function::new(locals);
        let mut operators = body.get_operators_reader()?;
        while !operators.eof() {
            let op = operators.read()?;
            use p::Operator::*;
            let store = match &op {
                I32Store { memarg } => Some((*memarg, 4, base + 1)),
                I32Store8 { memarg } => Some((*memarg, 1, base + 1)),
                I32Store16 { memarg } => Some((*memarg, 2, base + 1)),
                I64Store { memarg } => Some((*memarg, 8, base + 3)),
                I64Store8 { memarg } => Some((*memarg, 1, base + 3)),
                I64Store16 { memarg } => Some((*memarg, 2, base + 3)),
                I64Store32 { memarg } => Some((*memarg, 4, base + 3)),
                F32Store { memarg } => Some((*memarg, 4, base + 4)),
                F64Store { memarg } => Some((*memarg, 8, base + 5)),
                _ => None,
            };
            if let Some((arg, width, value)) = store {
                f.instruction(&I::LocalSet(value));
                f.instruction(&I::LocalTee(base));
                f.instruction(&I::LocalGet(value));
                f.instruction(&self.instruction(op)?);
                // Original store succeeded. Effective offsets therefore fit the
                // 64 MiB memory. Both ends cover an unaligned cross-page store.
                mark_address(&mut f, base, arg.offset as u32);
                if width > 1 {
                    mark_address(&mut f, base, (arg.offset as u32).wrapping_add(width - 1));
                }
            } else if matches!(
                op,
                MemoryCopy { .. } | MemoryFill { .. } | MemoryInit { .. }
            ) {
                f.instruction(&I::LocalSet(base + 2));
                f.instruction(&I::LocalSet(base + 1));
                f.instruction(&I::LocalTee(base));
                f.instruction(&I::LocalGet(base + 1));
                f.instruction(&I::LocalGet(base + 2));
                f.instruction(&self.instruction(op)?);
                mark_range(&mut f, base, base + 2);
            } else {
                f.instruction(&self.instruction(op)?);
            }
        }
        code.function(&f);
        Ok(())
    }
}

fn page_address(f: &mut enc::Function, address: u32, offset: u32) {
    f.instruction(&I::LocalGet(address));
    if offset != 0 {
        f.instruction(&I::I32Const(offset as i32));
        f.instruction(&I::I32Add);
    }
    f.instruction(&I::I32Const(12));
    f.instruction(&I::I32ShrU);
}
fn mark_address(f: &mut enc::Function, address: u32, offset: u32) {
    page_address(f, address, offset);
    f.instruction(&I::I32Const(1));
    f.instruction(&I::I32Store8(enc::MemArg {
        offset: 0,
        align: 0,
        memory_index: TRACKER_MEMORY,
    }));
}
fn mark_range(f: &mut enc::Function, address: u32, length: u32) {
    f.instruction(&I::LocalGet(length));
    f.instruction(&I::If(enc::BlockType::Empty));
    page_address(f, address, 0); // first page = destination in tracker
    f.instruction(&I::I32Const(1));
    f.instruction(&I::LocalGet(address));
    f.instruction(&I::LocalGet(length));
    f.instruction(&I::I32Add);
    f.instruction(&I::I32Const(1));
    f.instruction(&I::I32Sub);
    f.instruction(&I::I32Const(12));
    f.instruction(&I::I32ShrU); // last page
    page_address(f, address, 0);
    f.instruction(&I::I32Sub);
    f.instruction(&I::I32Const(1));
    f.instruction(&I::I32Add); // page count
    f.instruction(&I::MemoryFill(TRACKER_MEMORY));
    f.instruction(&I::End);
}
